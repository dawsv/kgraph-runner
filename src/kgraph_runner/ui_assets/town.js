
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const bootstrap = JSON.parse($('town-snapshot').textContent);
  const options = JSON.parse($('town-options').textContent);
  const viewport = $('viewport');
  const world = $('world');
  const NS = 'http://www.w3.org/2000/svg';
  const labels = {
    passed: 'Passed', cached: 'Cached', running: 'Running', failed: 'Failed', pending: 'Pending',
    circuit_broken: 'Circuit broken', awaiting_subagents: 'Awaiting subagents',
    budget_exceeded: 'Budget exceeded', stopped: 'Stopped', missing: 'Files missing'
  };
  const blockedStatuses = new Set(['failed', 'circuit_broken', 'budget_exceeded']);
  const frontierKinds = [
    {group: 'running', label: '⚡ Running: ', pin: 'RUNNING', pill: 'NOW'},
    {group: 'waiting', label: '🛡 Waiting: ', pin: 'WAITING', pill: 'WAITING'},
    {group: 'failed', label: '⚠ Stuck: ', pin: 'FAILED', pill: 'STUCK'},
    {group: 'pending', label: '▶ Next: ', pin: 'NEXT', pill: 'NEXT'}
  ];
  const roles = {
    worker: ['#e03a3e', '#ffce59', 'ϟ'],
    code: ['#e85d2a', '#ffbc70', '⌘'],
    verifier: ['#7c5cff', '#c1a7ff', '✓'],
    gate: ['#2bb673', '#a5e79a', '◈']
  };
  const districts = {
    subgraph: ['City block', '#87c7fd', '#e4f4ff', '#2d8cff'],
    loop_subgraph: ['Repair district', '#bb9bec', '#f1e5ff', '#805ad2'],
    splitter: ['Squad avenue', '#7fdbbb', '#e2faee', '#269981']
  };
  const camera = {x: 0, y: 0, scale: 1};
  const targets = new Map();
  const nodeIndex = new Map();
  const pointers = new Map();
  const rosterRows = new Map();
  const rosterList = $('roster-list');
  const rosterFilters = $('roster-filters');
  const roster = $('roster');
  const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
  const wideScreen = matchMedia('(min-width: 980px)');
  const dock = $('dock');
  const inspector = $('inspector');
  const attentionCard = $('attention-card');
  const stage = viewport.closest('.stage');
  const controls = $('fit-town').closest('.controls');
  const layoutButtons = [...controls.querySelectorAll('[data-layout]')];
  const signMetrics = document.createElement('canvas').getContext('2d');
  const signFont = 16;
  signMetrics.font = '850 ' + signFont + 'px system-ui';
  const rowGap = 78, waveGapY = 25, packedGap = 34;
  const mapInsetX = 18;
  // The ground axes recede in opposite directions. An inverse vertical
  // vector keeps extruded walls plumb in the projected town.
  const isoTilt = .27, isoSkew = -.39, isoDepth = .74;
  const isoDet = isoDepth - isoTilt * isoSkew;
  const plumbX = -isoSkew / isoDet, plumbY = 1 / isoDet;
  let layoutMode = options.layoutMode;
  let layoutSpace, townSpace;
  let snapshot, graphSignature, selected = null, gestureDragged = false, lastSkyTap;
  let fittedNodes = false, cameraMoved = false, cameraFrame = 0;
  let tasks = [], spotlights = new Map(), primarySpotlight, rosterFilter = 'all', hoveredTask = null;
  let attention, attentionSignature, inputGateSignature, preferredInputPath;
  let drillStack = [], breadcrumbSignature;
  let automatic = Boolean(options.live || options.events), timer, inFlight, eventStream, eventRetryTimer;
  let activeRunId = options.run, displayedRunId = options.run;
  let logRequest, logPath, qaView, qaTimer;

  function svg(tag, attrs, parent) {
    const el = document.createElementNS(NS, tag);
    Object.entries(attrs).forEach(([name, value]) => el.setAttribute(name, value));
    parent.append(el);
    return el;
  }
  function rect(parent, x, y, width, height, fill, attrs = {}) {
    return svg('rect', {x, y, width, height, fill, ...attrs}, parent);
  }
  function text(parent, value, x, y, size, attrs = {}) {
    const el = svg('text', {x, y, 'font-size': size, 'font-weight': 850, ...attrs}, parent);
    el.textContent = value;
    return el;
  }
  function path(parent, d, attrs) {
    return svg('path', {d, ...attrs}, parent);
  }
  function titleCase(value) {
    return String(value ?? '').replace(/_/g, ' ').replace(/^./, c => c.toUpperCase());
  }
  function statusLabel(status) {
    return Object.hasOwn(labels, status) ? labels[status] : titleCase(status);
  }
  function statusClass(status) {
    return 'status-' + (Object.hasOwn(labels, status) ? status : 'pending');
  }
  function shorten(value, max) {
    const chars = Array.from(String(value));
    return chars.length > max ? chars.slice(0, max - 1).join('') + '…' : chars.join('');
  }
  function seconds(value) {
    return value == null ? '—' : value + 's';
  }
  function collectLeafTasks(nodes, parents = [], tasks = []) {
    // Wave order respects dependencies. Stable sorting keeps the graph's
    // declaration order for siblings that can run together.
    for (const node of [...nodes].sort((a, b) => a.wave - b.wave)) {
      if (node.node_type === 'task') tasks.push({node, path: node.path, parents});
      else collectLeafTasks(node.children, [...parents, node.id], tasks);
    }
    return tasks;
  }
  function taskGroup(status) {
    if (status === 'passed' || status === 'cached') return 'done';
    if (status === 'awaiting_subagents') return 'waiting';
    if (blockedStatuses.has(status)) return 'failed';
    return status;
  }
  function docked() {
    return wideScreen.matches && document.body.classList.contains('full');
  }
  function syncInspector() {
    inspector.hidden = !targets.has(selected);
    roster.hidden = docked() && !inspector.hidden;
  }
  function layoutDock() {
    if (docked()) {
      dock.prepend(attentionCard);
      dock.append(inspector);
    } else {
      stage.before(attentionCard);
      stage.append(inspector);
    }
    const close = $('close-inspector');
    close.textContent = docked() ? '← Back to Tasks' : '×';
    close.setAttribute('aria-label', docked() ? 'Back to Tasks' : 'Close inspector');
    syncInspector();
  }
  wideScreen.addEventListener('change', layoutDock);
  layoutDock();
  function updateTaskProgress() {
    tasks = collectLeafTasks(snapshot.nodes);
    primarySpotlight = undefined;
    spotlights.clear();
    for (const kind of frontierKinds) {
      const matches = tasks.filter(({node}) => taskGroup(node.status) === kind.group);
      if (!matches.length) continue;
      primarySpotlight = matches[0];
      for (const task of kind.group === 'pending' ? matches.slice(0, 1) : matches) {
        spotlights.set(task.path, kind);
      }
      break;
    }

    const total = tasks.length;
    const done = tasks.filter(({node}) => taskGroup(node.status) === 'done').length;
    const percent = total ? Math.floor(100 * done / total) : 0;
    const count = done + ' / ' + total + ' tasks (' + percent + '%)';
    $('progress-count').textContent = count;
    $('roster-summary').textContent = 'All Tasks (' + done + '/' + total + ')';
    const bar = $('task-progress');
    bar.setAttribute('aria-valuenow', percent);
    bar.setAttribute('aria-valuetext', count);
    tasks.forEach(({node, path}, i) => {
      const segment = bar.children[i] || bar.appendChild(element('span', ''));
      segment.className = 'progress-segment ' + statusClass(node.status);
      segment.title = path + ' · ' + statusLabel(node.status);
    });
    while (bar.children.length > total) bar.lastElementChild.remove();

    const nowAt = $('now-at');
    nowAt.disabled = !primarySpotlight;
    nowAt.className = 'now-at' + (primarySpotlight ? ' ' + statusClass(primarySpotlight.node.status) : '');
    $('now-at-text').textContent = primarySpotlight ? spotlights.get(primarySpotlight.path).label + primarySpotlight.path :
      '✓ All done (' + done + '/' + total + ')';
  }
  $('now-at').addEventListener('click', () => {
    if (primarySpotlight) focusNode(primarySpotlight.path);
  });
  const attentionLabels = {
    decision: '🛡 NEEDS YOUR DECISION', waiting: '🛡 HERO NEEDED!', stuck: '⚠ STUCK',
    running: '⚡ RUNNING NOW', done: '✓ ALL DONE'
  };
  function legacyAttention() {
    const group = taskGroup(snapshot.status);
    if (group === 'done' && tasks.length && !primarySpotlight) {
      return {kind: 'done', headline: 'Workflow complete'};
    }
    const node = primarySpotlight?.node;
    if (node && ['running', 'waiting', 'failed'].includes(group) && taskGroup(node.status) === group) {
      return {kind: group === 'failed' ? 'stuck' : group, headline: titleCase(node.id),
        detail: node.detail || node.error_signature, node_path: node.path};
    }
    return null;
  }
  function renderAttention() {
    let next = snapshot.attention ?? (Object.hasOwn(snapshot, 'attention') ? null : legacyAttention());
    const preferred = nodeIndex.get(preferredInputPath);
    const gate = preferred?.input_gate;
    if (gate && !gate.submitted && preferred.path !== next?.node_path) {
      next = {kind: 'decision', headline: preferred.title || titleCase(preferred.id),
        detail: preferred.detail, node_path: preferred.path, input_gate: gate, packet_paths: next?.packet_paths};
    } else if (!gate || gate.submitted) {
      preferredInputPath = null;
    }
    const signature = JSON.stringify(next);
    if (signature === attentionSignature) return false;
    attention = next;
    attentionSignature = signature;
    attentionCard.hidden = !attention || !Object.hasOwn(attentionLabels, attention.kind);
    renderInputGate(attention?.input_gate);
    if (attentionCard.hidden) return true;
    attentionCard.dataset.kind = attention.kind;
    $('attention-tag').textContent = attentionLabels[attention.kind];
    $('attention-headline').textContent = attention.headline;
    const detail = $('attention-detail');
    detail.textContent = attention.detail || '';
    detail.hidden = !attention.detail;
    const items = Array.isArray(attention.items) ? [...attention.items] : [];
    if (Array.isArray(attention.packet_paths) && attention.packet_paths.length > 1) {
      items.push(attention.packet_paths.length + ' gates awaiting input');
    }
    const list = $('attention-items');
    list.replaceChildren(...items.map(item => element('li', item)));
    list.hidden = !items.length;
    $('attention-focus').hidden = !attention.node_path;
    return true;
  }
  function gateChoice(label, {type, name, value, checked = false, recommended = false}) {
    const row = element('label', '');
    row.className = 'gate-choice';
    const input = document.createElement('input');
    Object.assign(input, {type, name, value, checked});
    row.append(input, element('span', label));
    if (recommended) {
      const star = element('strong', '★ Recommended');
      star.className = 'gate-pill';
      row.append(star);
    }
    return {row, input};
  }

  function gateText(label, tag, limit) {
    const row = element('label', '');
    row.className = 'gate-text';
    const input = document.createElement(tag);
    if (limit != null) input.maxLength = limit;
    if (tag === 'textarea') input.rows = 2;
    row.append(element('span', label), input);
    return {row, input};
  }

  function gateQuestion(question, index, readOnly) {
    const card = element('fieldset', '');
    const legend = element('legend', '');
    if (question.ticket) {
      const ticket = element('span', question.ticket);
      ticket.className = 'gate-pill';
      legend.append(ticket, ' ');
    }
    legend.append(question.question);
    card.append(legend);

    const choices = question.options;
    const multiple = question.multi_select && choices.length > 0;
    const name = 'gate-question-' + index;
    const controls = [];
    for (const value of choices.length ? choices : [question.recommended].filter(Boolean)) {
      const recommended = value === question.recommended;
      const label = choices.length ? value : '★ Accept Recommended · ' + value;
      const {row, input} = gateChoice(label, {name, value, type: multiple ? 'checkbox' : 'radio',
        checked: recommended && !multiple, recommended: recommended && Boolean(choices.length)});
      card.append(row);
      controls.push(input);
    }
    let custom, customChoice;
    if (!controls.length || (question.allow_note && !readOnly)) {
      custom = gateText(controls.length ? (multiple ? 'Additional answer' : 'Custom answer') : 'Answer',
        controls.length ? 'input' : 'textarea', 8000);
      if (controls.length && !multiple) {
        customChoice = gateChoice('Custom answer', {name, value: '', type: 'radio'});
        card.append(customChoice.row);
        custom.row.firstElementChild.textContent = 'Enter your answer';
        custom.input.addEventListener('focus', () => { customChoice.input.checked = true; });
        custom.input.addEventListener('input', () => { customChoice.input.checked = true; });
      }
      card.append(custom.row);
    }
    const answer = () => {
      const selected = controls.filter(input => input.checked).map(input => input.value);
      const typed = custom?.input.value.trim();
      if (multiple) return typed ? [...selected, typed] : selected;
      return !controls.length || customChoice?.input.checked ? typed || '' : selected[0] || '';
    };
    return {card, answer};
  }

  function renderInputGate(gate) {
    const container = $('input-gate');
    const submitted = gate?.submitted;
    const signature = gate ? JSON.stringify([displayedRunId, gate.node_path, gate.packet_hash,
      gate.can_respond, submitted?.answered_at]) : '';
    if (signature === inputGateSignature) return;
    inputGateSignature = signature;
    container.replaceChildren();
    container.hidden = !gate;
    attentionCard.toggleAttribute('data-input', Boolean(gate));
    if (!gate) return;

    const canRespond = Boolean(options.respond && gate.can_respond);
    const form = element('form', '');
    form.className = 'gate-form';
    const questions = (gate.questions || []).map((question, index) => ({
      id: question.id, ...gateQuestion(question, index, !canRespond)
    }));
    for (const question of questions) form.append(question.card);
    const approval = gate.gate_type === 'approval' || gate.gate_type === 'hybrid';
    const fields = [], rejected = [];
    if (approval) {
      const release = element('fieldset', '');
      release.append(element('legend', 'Release approval'));
      if (gate.review_summary) {
        const review = element('p', gate.review_summary);
        review.className = 'gate-review';
        release.append(review);
      }
      for (const field of gate.approved_fields || []) {
        const {row, input} = gateChoice(field.label, {type: 'checkbox', checked: field.default !== false});
        fields.push([field.key, input]);
        release.append(row);
      }
      form.append(release);
      if (canRespond && gate.bugs?.length) {
        const list = element('details', '');
        list.className = 'gate-bugs';
        list.append(element('summary', 'Rejected bugs'));
        for (const bug of gate.bugs) {
          const {row, input} = gateChoice(bug.title === bug.id ? bug.id : bug.title + ' · ' + bug.id,
            {type: 'checkbox', value: bug.id});
          list.append(row);
          rejected.push(input);
        }
        form.append(list);
      }
    }

    if (submitted) {
      const stamp = new Date(submitted.answered_at);
      const date = Number.isFinite(stamp.valueOf()) ? ' · ' + stamp.toLocaleString() : '';
      const banner = element('div', '');
      banner.className = 'gate-submitted';
      banner.setAttribute('role', 'status');
      banner.append(element('strong', '✓ Submitted by ' + (submitted.approved_by || 'reviewer') + date));
      if (submitted.summary) banner.append(element('p', submitted.summary));
      container.append(banner);
      if (canRespond) {
        const edit = element('button', 'Edit & Resubmit');
        edit.type = 'button';
        edit.setAttribute('aria-expanded', 'false');
        edit.setAttribute('aria-controls', 'input-gate-form');
        edit.addEventListener('click', () => {
          form.hidden = !form.hidden;
          edit.setAttribute('aria-expanded', String(!form.hidden));
          edit.textContent = form.hidden ? 'Edit & Resubmit' : 'Cancel Edit';
        });
        container.append(edit);
        form.hidden = true;
      }
    }
    form.id = 'input-gate-form';
    container.append(form);
    if (!canRespond) {
      for (const control of form.elements) control.disabled = true;
      if (!submitted) form.append(element('p', 'Read-only input preview'));
      return;
    }
    const responder = gateText('Respond as', 'input', 80);
    let savedResponder = '';
    try { savedResponder = localStorage.getItem('kgraph.reviewer') || ''; } catch {}
    responder.input.value = savedResponder || 'Reviewer';
    responder.input.placeholder = 'Your name';
    responder.input.required = true;
    const notes = gateText('Notes (optional)', 'textarea', 8000);
    form.append(responder.row, notes.row);
    const actions = element('div', '');
    actions.className = 'actions';
    const action = (label, value, primary) => {
      const button = element('button', label);
      Object.assign(button, {type: 'submit', value});
      if (primary) button.className = 'primary';
      actions.append(button);
    };
    if (approval) {
      action('✓ Approve', 'approve', true);
      action('✕ Request Fixes', 'reject', false);
    } else {
      action('Submit Decision', 'submit', true);
    }
    const status = element('p', '');
    status.className = 'gate-status';
    status.setAttribute('role', 'status');
    form.append(actions, status);
    form.addEventListener('submit', async event => {
      event.preventDefault();
      if (options.runs && activeRunId !== displayedRunId) {
        status.textContent = 'Wait for the selected run to load.';
        return;
      }
      const payload = {node_path: gate.node_path, packet_hash: gate.packet_hash,
        submitted_at: submitted?.answered_at ?? null, approved_by: responder.input.value, notes: notes.input.value};
      if (questions.length) payload.decisions = Object.fromEntries(questions.map(q => [q.id, q.answer()]));
      if (approval) {
        payload.approved = event.submitter?.value !== 'reject';
        payload.approved_fields = Object.fromEntries(fields.map(([key, input]) => [key, input.checked]));
        if (!payload.approved) payload.rejected_bugs = rejected.filter(input => input.checked).map(input => input.value);
      }
      const url = options.runs ? options.respond + '/' + encodeURIComponent(displayedRunId) + '/respond' : options.respond;
      for (const button of actions.children) button.disabled = true;
      status.removeAttribute('data-error');
      status.textContent = 'Submitting…';
      try {
        const response = await fetch(url, {method: 'POST', headers: {'Content-Type': 'application/json'},
          body: JSON.stringify(payload)});
        const data = await response.json();
        if (!response.ok) throw new Error(data.error || 'HTTP ' + response.status);
        try {
          const name = responder.input.value.trim();
          if (name) localStorage.setItem('kgraph.reviewer', name);
        } catch {}
        if (inputGateSignature === signature && activeRunId === displayedRunId) {
          renderInputGate({...gate, submitted: {answered_at: data.answered_at,
            approved_by: data.decision.approved_by, summary: data.summary}});
          refresh();
        }
      } catch (error) {
        status.dataset.error = '';
        status.textContent = error.message || 'Could not submit the decision.';
      } finally {
        for (const button of actions.children) button.disabled = false;
      }
    });
  }

  $('attention-focus').addEventListener('click', () => focusNode(attention?.node_path));

  function indexNodes(nodes) {
    for (const node of nodes) {
      nodeIndex.set(node.path, node);
      indexNodes(node.children || []);
      indexNodes(node.sub_graph?.nodes || []);
    }
  }

  function subProgress(node, graph) {
    if (node.sub_progress) return node.sub_progress;
    const children = graph.nodes;
    return {total: children.length, done: children.filter(child => taskGroup(child.status) === 'done').length,
      running: children.filter(child => child.status === 'running').length};
  }

  function stageGraph() {
    const path = drillStack.at(-1);
    return path ? subGraph(nodeIndex.get(path)) || {nodes: [], edges: []} : snapshot;
  }

  function renderBreadcrumbs() {
    const bar = $('drill-breadcrumbs');
    const node = nodeIndex.get(drillStack.at(-1));
    const graph = subGraph(node);
    const progress = graph ? subProgress(node, graph) : null;
    const names = ['Town', ...drillStack.map(path => nodeIndex.get(path)?.title || nodeIndex.get(path)?.id)];
    const signature = JSON.stringify([drillStack, names, progress]);
    if (signature === breadcrumbSignature) return;
    breadcrumbSignature = signature;
    const focused = bar.contains(document.activeElement) && document.activeElement.dataset.depth;
    bar.hidden = !drillStack.length;
    bar.replaceChildren();
    if (bar.hidden) return;
    const crumb = (label, depth) => {
      const button = element('button', label);
      button.type = 'button';
      button.dataset.depth = depth;
      button.addEventListener('click', () => drillTo(drillStack.slice(0, depth)));
      return button;
    };
    const back = crumb('← Back', drillStack.length - 1);
    back.id = 'drill-back';
    bar.append(back);
    for (const [depth, name] of names.entries()) {
      if (depth > 0) {
        const arrow = element('span', '›');
        arrow.setAttribute('aria-hidden', 'true');
        bar.append(arrow);
      }
      const current = depth === drillStack.length;
      const item = current ? element('strong', name) : crumb(name, depth);
      if (current) item.setAttribute('aria-current', 'location');
      bar.append(item);
    }
    if (progress) {
      const counts = element('span', progress.done + '/' + progress.total + ' done');
      counts.className = 'gate-pill';
      bar.append(counts);
    }
    if (focused !== false) [...bar.querySelectorAll('button')].find(button => button.dataset.depth === focused)?.focus();
  }

  function drillTo(stack, focusPath = null, focusOptions = {scroll: false}) {
    const previous = drillStack.at(-1);
    select(null);
    drillStack = stack;
    redrawTown();
    fitTown();
    $('empty').hidden = Boolean(stageGraph().nodes.length);
    if (focusPath) {
      focusNode(focusPath, focusOptions);
      targets.get(focusPath)?.target.focus({preventScroll: true});
    } else if (drillStack.length) {
      $('drill-back')?.focus({preventScroll: true});
    } else {
      targets.get(previous)?.target.focus({preventScroll: true});
    }
  }

  function openSubGraph(node, focusPath, focusOptions) {
    if (!subGraph(node)) return;
    const ancestors = drillStack.filter(path => node.path.startsWith(path + '/'));
    drillTo([...ancestors, node.path], focusPath, focusOptions);
  }

  // Measure inside-out so wide districts count at their actual width.
  function measureNode(node) {
    if (node.node_type === 'task') return {node, w: 202, h: 152};
    const parallel = node.node_type === 'splitter';
    const inside = measureLevel(node.children, node.edges, {parallel});
    const inset = layoutMode === 'iso' ? 91 : 74;
    return {node, inside, w: Math.max(258, inside.w + 36), h: Math.max(135, inside.h + inset)};
  }

  function subGraph(node) {
    if (node?.sub_graph?.nodes?.length) return node.sub_graph;
    if (node?.children?.length) return {nodes: node.children, edges: node.edges || [], parallel: node.node_type === 'splitter'};
    return null;
  }

  function buildingStatus(node) {
    if (node.status !== 'awaiting_subagents') return statusLabel(node.status).toUpperCase();
    if ((node.input_gate && !node.input_gate.submitted) ||
        (attention?.kind === 'decision' && attention.node_path === node.path)) return 'INPUT NEEDED';
    const progress = node.sub_progress;
    return progress?.running > 0 ? 'ACTIVE ' + progress.done + '/' + progress.total : 'WAITING';
  }

  function splitterRows(branches, gapX, gapY) {
    if (!branches.length) return [[]];
    const maxCols = layoutMode === 'strip' ? branches.length : Math.min(branches.length, 4);
    const minCols = layoutMode === 'strip' ? maxCols : 1;
    let bestRows, bestSize = Infinity;
    for (let cols = minCols; cols <= maxCols; cols++) {
      const widths = Array(cols).fill(0);
      branches.forEach((item, i) => {
        widths[i % cols] = Math.max(widths[i % cols], item.w);
      });
      const rows = [];
      let height = 0;
      for (let i = 0; i < branches.length; i += cols) {
        const row = branches.slice(i, i + cols).map((item, col) => ({group: [item], w: widths[col], h: item.h}));
        rows.push(row);
        height += Math.max(...row.map(wave => wave.h));
      }
      height += gapY * (rows.length - 1);
      const width = widths.reduce((sum, w) => sum + w, gapX * (cols - 1));
      const size = Math.max(width / 2.1, height);
      if (size < bestSize) {
        bestSize = size;
        bestRows = rows;
      }
    }
    return bestRows;
  }

  function wrapWaves(waves, gap, root, place) {
    if (layoutMode === 'strip' || waves.length < (root ? 2 : 3)) return place([waves]);
    const widths = [0];
    for (const wave of waves) widths.push(widths.at(-1) + wave.w + gap);

    // For each row count the total width is fixed, so minimizing squared
    // widths also minimizes variance. Reuse the partitions across counts.
    const starts = [[], Array(waves.length + 1).fill(0)];
    let costs = widths.map(width => (width - gap) ** 2);
    let bestRows = [waves];
    const initial = place(bestRows);
    const {single, wrapped} = townSpace;
    const target = root ? single : wrapped;
    let bestScale = Math.min(target.w / initial.w, target.h / initial.h);
    let bestShape = Math.abs(Math.log(initial.w / initial.h / target.aspect));
    const minHeight = Math.min(...waves.map(wave => wave.h));
    for (let count = 2; count <= waves.length; count++) {
      if (wrapped.h / (count * minHeight + (count - 1) * rowGap) < bestScale) break;
      const nextCosts = [], rowStarts = [];
      for (let end = count; end <= waves.length; end++) {
        let cost = Infinity;
        for (let start = count - 1; start < end; start++) {
          const width = widths[end] - widths[start] - gap;
          const next = costs[start] + width ** 2;
          if (next < cost) {
            cost = next;
            rowStarts[end] = start;
          }
        }
        nextCosts[end] = cost;
      }
      costs = nextCosts;
      starts.push(rowStarts);

      const rows = [];
      let end = waves.length;
      for (let row = count; row > 0; row--) {
        const start = starts[row][end];
        rows.push(waves.slice(start, end));
        end = start;
      }
      rows.reverse();
      const candidate = place(rows);
      const scale = Math.min(wrapped.w / candidate.w, wrapped.h / candidate.h);
      const shape = Math.abs(Math.log(candidate.w / candidate.h / wrapped.aspect));
      if (scale > bestScale || (scale === bestScale && shape < bestShape)) {
        bestRows = rows;
        bestScale = scale;
        bestShape = shape;
      }
    }
    // Restore the winning coordinates after scoring the other arrangements.
    return starts.length === 2 ? initial : place(bestRows);
  }

  function measureLevel(nodes, edges, {parallel = false, root = false} = {}) {
    const items = new Map(nodes.map(node => [node.path, measureNode(node)]));
    const gapX = parallel ? 24 : 74;
    const rowSpacing = parallel ? 20 : rowGap;
    const place = rows => placeRows(rows, edges, items, gapX, rowSpacing, parallel);
    if (parallel) {
      for (const item of items.values()) item.column = 0;
      return place(splitterRows([...items.values()], gapX, rowSpacing));
    }
    const columns = new Map();
    for (const item of items.values()) {
      const wave = item.node.wave;
      if (!columns.has(wave)) columns.set(wave, []);
      columns.get(wave).push(item);
    }
    const waves = [...columns.entries()].sort(([a], [b]) => a - b).map(([, group], index) => {
      const packed = layoutMode !== 'strip' && group.length >= 4 && group.every(item => item.node.node_type === 'task');
      group.forEach((item, i) => {
        item.column = index;
        item.innerEntry = packed && i % 2 === 1;
        item.innerExit = packed && i % 2 === 0;
      });
      const cellWidth = Math.max(...group.map(item => item.w));
      const line = packed ? group.filter((_, i) => i % 2 === 0) : group;
      return {
        group, cellWidth, packed,
        w: packed ? cellWidth * 2 + packedGap : cellWidth,
        h: line.reduce((sum, item) => sum + item.h, 0) + waveGapY * (line.length - 1)
      };
    });
    return wrapWaves(waves, gapX, root, place);
  }

  function placeRows(rowGroups, edges, items, gapX, rowSpacing, parallel) {
    const rows = rowGroups.map((rowWaves, row) => {
      rowWaves.forEach((wave, col) => {
        for (const item of wave.group) Object.assign(item, {row, col});
      });
      return {waves: rowWaves, bypasses: 0, avenues: 0};
    });
    let express = 0;
    const connections = [];
    const retryConnections = [];
    for (const edge of edges) {
      const from = items.get(edge.from), to = items.get(edge.to);
      if (!from || !to) continue;
      if (edge.kind === 'retry') {
        retryConnections.push({from, to, label: edge.label, lane: retryConnections.length});
        continue;
      }
      if (from.column >= to.column) continue;
      const connection = {from, to};
      if (to.row > from.row) {
        connection.exit = rows[from.row].avenues++;
        if (to.row > from.row + 1) {
          connection.entry = rows[to.row - 1].avenues++;
          connection.express = express++;
        }
      } else if (to.col > from.col + 1 || from.innerExit || to.innerEntry) {
        connection.bypass = rows[from.row].bypasses++;
      }
      connections.push(connection);
    }
    const wrapped = !parallel && rows.length > 1;
    const retryGutter = retryConnections.length ? 34 + (retryConnections.length - 1) * 10 : 0;
    const left = (express ? 57 + 7 * (express - 1) : wrapped ? 32 : 0) + retryGutter;
    const right = (wrapped ? 28 : 0) + retryGutter;
    let w = 0, y = 0;
    for (const row of rows) {
      row.top = y;
      const overhead = row.bypasses ? 16 + row.bypasses * 11 : 0;
      const tallest = Math.max(0, ...row.waves.map(wave => wave.h));
      let x = left;
      for (const wave of row.waves) {
        let laneY = y + overhead + (tallest - wave.h) / 2;
        wave.group.forEach((item, i) => {
          const itemX = item.innerEntry ? x + wave.cellWidth + packedGap : x;
          Object.assign(item, {x: itemX, y: laneY, columnWidth: wave.cellWidth ?? wave.w});
          if (!wave.packed || i % 2 === 1) laneY += item.h + waveGapY;
        });
        x += wave.w + gapX;
      }
      w = Math.max(w, x - gapX + right);
      row.bottom = y + tallest + overhead;
      y = row.bottom + rowSpacing;
    }
    const contentBottom = y - rowSpacing;
    const retryClearance = retryConnections.length ? 72 + (retryConnections.length - 1) * 44 : 0;
    return {items, connections, retryConnections, rows, left, w,
      contentBottom, h: contentBottom + retryClearance, parallel};
  }

  function anchor(item, output) {
    const task = item.node.node_type === 'task';
    return {
      x: item.x + (output ? item.w - (task ? 0 : 1) : (task ? 6 : 1)),
      y: item.y + (task ? 100 : item.h / 2)
    };
  }
  function roundedRoute(points) {
    const corners = points.filter(([x, y], i) => i === 0 || x !== points[i - 1][0] || y !== points[i - 1][1]);
    const toward = ([x, y], [nextX, nextY], radius) => {
      const distance = Math.abs(nextX - x) + Math.abs(nextY - y);
      const ratio = Math.min(radius / distance, .5);
      return (x + (nextX - x) * ratio) + ' ' + (y + (nextY - y) * ratio);
    };
    let d = 'M' + corners[0].join(' ');
    for (let i = 1; i < corners.length - 1; i++) {
      const corner = corners[i];
      d += 'L' + toward(corner, corners[i - 1], 10) +
        'Q' + corner.join(' ') + ' ' + toward(corner, corners[i + 1], 10);
    }
    return d + 'L' + corners.at(-1).join(' ');
  }

  function avenueY(row, lane) {
    const spacing = Math.min(9, 46 / Math.max(1, row.avenues - 1));
    return row.bottom + rowGap / 2 + (lane - (row.avenues - 1) / 2) * spacing;
  }

  function drawRetryConnections(group, level, projection) {
    const active = [...level.items.values()].some(({node}) =>
      node.status === 'awaiting_subagents' || blockedStatuses.has(node.status));
    const iso = layoutMode === 'iso';
    for (const {from, to, label, lane} of level.retryConnections) {
      const track = svg('g', {class: active ? 'retry-active' : 'retry-route', transform: projection}, group);
      const left = 12 + 9 * lane, right = level.w - left;
      const bottom = level.contentBottom + 44 + 44 * lane;
      const fromTask = from.node.node_type === 'task', toTask = to.node.node_type === 'task';
      const a = [from.x + from.w - (fromTask && !iso ? 2 : 5), from.y + (fromTask ? 132 : from.h - 40)];
      const b = [to.x + (toTask && iso ? 8 : 0), to.y + (toTask ? 132 : to.h - 40)];
      const depart = from.x + from.columnWidth + 12, approach = to.x - 12;
      const lower = level.rows[from.row].bottom + 16, upper = level.rows[to.row].bottom + 16;
      const route = roundedRoute([a, [depart, a[1]], [depart, lower], [right, lower], [right, bottom],
        [left, bottom], [left, upper], [approach, upper], [approach, b[1]], b]);
      path(track, route, {class: 'retry-outline'});
      path(track, route, {class: 'retry-wire', 'marker-end': 'url(#retry-arrow)'});
      const caption = '↻ ' + titleCase(label || 'If fails → retry');
      const review = from.node.kind === 'gate' || /reject|review|approval/i.test(label || '');
      const size = review ? 26 : 21;
      const height = size + 12;
      const captionWidth = Math.ceil(signMetrics.measureText(caption).width * size / signFont);
      const width = Math.min(captionWidth + 24, right - left - 104);
      const middle = (left + right) / 2;
      const fit = width < captionWidth + 24 ? {textLength: width - 24, lengthAdjust: 'spacingAndGlyphs'} : {};
      rect(track, middle - width / 2, bottom - height / 2, width, height, '#fff1cc',
        {rx: height / 2, class: 'retry-label-bg'});
      text(track, caption, middle, bottom + size / 3, size,
        {class: 'retry-label-text', 'text-anchor': 'middle', ...fit});
      const villain = review ? 'villain-thanos' : 'villain-doom';
      drawHero(track, villain, middle + width / 2 + 6, bottom - 18, 34, 36);
    }
  }

  function drawLevel(group, level, projection = '') {
    const routeTransform = projection ? {transform: projection} : {};
    for (const {from, to, bypass, exit, entry, express} of level.connections) {
      const a = anchor(from, true);
      const b = anchor(to, false);
      const outside = from.x + from.columnWidth + (from.innerExit ? 17 : 22);
      const approach = to.x - (to.innerEntry ? 17 : 26);
      let d;
      if (to.row > from.row) {
        const corridorY = avenueY(level.rows[from.row], exit);
        const route = [[a.x, a.y], [outside, a.y], [outside, corridorY]];
        let arrivalY = corridorY;
        if (entry !== undefined) {
          const through = level.left - 48 - express * 7;
          arrivalY = avenueY(level.rows[to.row - 1], entry);
          route.push([through, corridorY], [through, arrivalY]);
        }
        route.push([approach, arrivalY], [approach, b.y], [b.x - 8, b.y]);
        d = roundedRoute(route);
      } else if (bypass === undefined) {
        const middle = (outside + b.x) / 2;
        d = 'M' + a.x + ' ' + a.y + 'H' + outside + 'C' + middle + ' ' + a.y + ' ' +
          middle + ' ' + b.y + ' ' + (b.x - 8) + ' ' + b.y;
      } else {
        const lane = level.rows[from.row].top + 9 + 11 * bypass;
        d = roundedRoute([[a.x, a.y], [outside, a.y], [outside, lane],
          [approach, lane], [approach, b.y], [b.x - 8, b.y]]);
      }
      path(group, d, {class: 'link-outline', ...routeTransform});
      path(group, d, {class: 'link-wire', 'marker-end': 'url(#street-arrow)', ...routeTransform});
      if (from.node.status === 'running' || to.node.status === 'running') {
        path(group, d, {class: 'link-pulse', ...routeTransform});
      }
    }
    if (level.retryConnections.length) drawRetryConnections(group, level, projection);
    for (const item of level.items.values()) {
      const wrapper = svg('g', {class: 'node ' + statusClass(item.node.status),
        transform: projection + 'translate(' + item.x + ' ' + item.y + ')'}, group);
      const target = svg('g', {class: 'node-target', tabindex: 0, role: 'button',
        'aria-label': item.node.path + ', ' + statusLabel(item.node.status)}, wrapper);
      const mission = attention?.node_path === item.node.path ? attention.headline : '';
      const nested = subGraph(item.node);
      const detail = [mission, item.node.detail || item.node.error_signature,
        nested ? 'Open the Inspector for sub-tasks or double-click to zoom in' : ''].filter(Boolean).join(' · ');
      if (detail) svg('title', {}, target).textContent = shorten(detail, 200);
      targets.set(item.node.path, {target, node: item.node, w: item.w, h: item.h});
      rect(target, 0, 0, item.w, item.h, 'transparent', {class: 'hit'});
      if (item.inside) drawDistrict(wrapper, target, item);
      else if (layoutMode === 'iso') drawIsoBuilding(target, item.node);
      else drawBuilding(target, item.node);
      rect(target, 2, 2, item.w - 4, item.h - 4, 'none', {rx: 10, class: 'selection'});
      const inspect = () => item.node.node_type === 'task' ? focusNode(item.node.path) : select(item.node.path);
      target.addEventListener('click', event => { if (!gestureDragged || !event.detail) inspect(); });
      if (nested) target.addEventListener('dblclick', event => {
        event.preventDefault();
        if (!gestureDragged) openSubGraph(item.node);
      });
      target.addEventListener('keydown', event => {
        if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault();
          inspect();
        }
      });
    }
  }

  function point(x, y, z = 0) {
    return [x - z * plumbX, y - z * plumbY];
  }

  function polygon(group, vertices, fill, attrs = {}) {
    return svg('polygon', {points: vertices.map(vertex => vertex.join(',')).join(' '),
      fill, ...attrs}, group);
  }

  function drawIsoBlock(group, x, y, w, d, base, height, colors) {
    const right = x + w, front = y + d, roof = base + height;
    const [topColor, rightColor, frontColor] = colors;
    const line = {class: 'comic-stroke'};
    polygon(group, [point(right, y, roof), point(right, front, roof),
      point(right, front, base), point(right, y, base)], rightColor, line);
    polygon(group, [point(x, front, roof), point(right, front, roof),
      point(right, front, base), point(x, front, base)], frontColor, line);
    polygon(group, [point(x, y, roof), point(right, y, roof),
      point(right, front, roof), point(x, front, roof)], topColor, line);
  }

  function drawDistrict(wrapper, target, item) {
    const {node, inside, w, h} = item;
    const [label, bright, paper, accent] = districts[node.node_type] || districts.subgraph;
    const iso = layoutMode === 'iso';
    if (iso) {
      rect(target, 8, 19, w - 9, h - 20, '#172033', {rx: 6, opacity: .27});
      drawIsoBlock(target, 7, 19, w - 14, h - 25, 0, 12, [paper, accent, bright]);
      rect(target, 3.5, 5.2, w - 14, 50, bright, {class: 'comic-stroke'});
    } else {
      rect(target, 5, 5, w - 5, h - 5, '#172033', {rx: 13});
      rect(target, 1.5, 1.5, w - 7, h - 7, paper, {rx: 13, class: 'comic-stroke'});
      path(target, 'M14 2H' + (w - 18) + 'q12 0 12 12v40H2V14Q2 2 14 2Z',
        {fill: bright, class: 'comic-stroke'});
    }
    text(target, shorten(node.id, Math.max(13, Math.floor((w - 106) / 10))), 16, 25, 21);
    text(target, label.toUpperCase() + (subGraph(node) ? ' ↗' : ''), 17, 43, 10, {'letter-spacing': 1.4});
    const status = buildingStatus(node);
    const badgeWidth = Math.max(66, status.length * 6 + 14);
    rect(target, w - badgeWidth - 15, 14, badgeWidth, 22, 'white', {rx: 7, class: 'comic-stroke status-fill'});
    text(target, status, w - badgeWidth / 2 - 15, 29, 9, {'text-anchor': 'middle'});
    if (node.node_type === 'loop_subgraph') {
      rect(target, 10, 59, w - 25, h - (iso ? 87 : 70), 'none', {rx: 15, class: 'loop-track'});
      const badge = '↻ max ' + (node.max_retries ?? '—');
      rect(target, w - 80, 39, 64, 20, '#ffefd1', {rx: 6, class: 'comic-stroke'});
      text(target, badge, w - 48, 53, 10, {'text-anchor': 'middle'});
    }
    const offsetX = (w - inside.w) / 2;
    if (inside.parallel) {
      for (const branch of inside.items.values()) {
        const x = offsetX + branch.x - 5, y = 63 + branch.y - 4;
        rect(target, x, y, branch.w + 10, branch.h + 8, '#c4f0e1', {rx: 8});
        path(target, 'M' + (x + 2) + ' ' + (y + branch.h) + 'V' + (y + 8) +
          'Q' + (x + 2) + ' ' + (y + 2) + ' ' + (x + 8) + ' ' + (y + 2) + 'H' + (x + branch.w + 2), {
          fill: 'none', stroke: accent, 'stroke-width': 2
        });
      }
    }
    if (inside.items.size) {
      drawLevel(svg('g', {transform: 'translate(' + offsetX + ' 63)'}, wrapper), inside);
    } else {
      text(target, 'No stages yet', w / 2, 100, 13, {'text-anchor': 'middle'});
    }
  }

  function heroFor(node, kind) {
    const id = String(node.id || '').toLowerCase();
    if (node.status === 'budget_exceeded' || String(node.error_signature || '').includes('rejected')) {
      return 'villain-thanos';
    }
    if (node.status === 'failed' || node.status === 'circuit_broken') return 'villain-doom';
    if (kind === 'gate') return 'gate';
    if (kind === 'verifier') {
      if (/(build|validate|batch|review|shield|cap|check)/.test(id)) return 'captain';
      if (/(test|run|qa|runtime)/.test(id)) return 'thor';
      return 'verifier';
    }
    if (kind === 'worker') {
      if (/(fix|commit|upload|publish|thor|bolt|repair|rig)/.test(id)) return 'thor';
      return 'worker';
    }
    if (kind === 'code') {
      if (/(prepare|release|package|shield|cap)/.test(id)) return 'captain';
      return 'code';
    }
    return kind;
  }

  function drawHero(group, heroId, x, y, width, height) {
    const hero = svg('g', {transform: 'translate(' + x + ' ' + y + ')', class: 'hero-art'}, group);
    const href = heroId.startsWith('villain-') ? '#' + heroId : '#hero-' + heroId;
    svg('use', {href, x: 0, y: 0, width, height,
      class: heroId === 'worker' || heroId === 'thor' ? 'flyer' : ''}, hero);
  }

  function drawBuildingName(group, id, x, y, w) {
    const name = id.replace(/[_-]+/g, ' ').trim() || id;
    const available = w - 10;
    const widthOf = line => signMetrics.measureText(line).width;
    let lines = [name];
    if (widthOf(name) > available) {
      const words = name.split(/\s+/);
      let widest = Infinity;
      for (let split = 1; split < words.length; split++) {
        const pair = [words.slice(0, split).join(' '), words.slice(split).join(' ')];
        const width = Math.max(...pair.map(widthOf));
        if (width < widest) {
          widest = width;
          lines = pair;
        }
      }
    }
    rect(group, x, y, w, 35, '#fff6e4', {rx: 5, class: 'comic-stroke'});
    lines.forEach((line, i) => {
      const fitLine = widthOf(line) > available ? {textLength: available, lengthAdjust: 'spacingAndGlyphs'} : {};
      text(group, line, x + w / 2, y + (lines.length === 1 ? 23 : 15 + i * 15),
        signFont, {'text-anchor': 'middle', ...fitLine});
    });
  }

  function drawIsoBuilding(group, node) {
    const kind = Object.hasOwn(roles, node.kind) ? node.kind : 'code';
    const [paint, trim] = roles[kind];
    const line = {class: 'comic-stroke'};
    const facade = 'matrix(1 0 ' + plumbX + ' ' + plumbY + ' ';
    rect(group, 9, 119, 187, 30, '#172033', {rx: 5, opacity: .28});
    drawIsoBlock(group, 9, 118, 183, 26, 0, 6, ['#ffefcf', '#b37c62', '#e1b695']);
    drawIsoBlock(group, 33, 84, 122, 37, 6, 62, [trim, '#45516a', paint]);
    const [x, y] = point(33, 121, 68);
    const face = svg('g', {transform: facade + x + ' ' + y + ')'}, group);
    drawBuildingName(face, node.title || node.id, 4, 4, 114);
    for (const wx of [10, 47, 84]) {
      rect(face, wx, 44, 28, 13, '#fff', {rx: 2, ...line});
      rect(face, wx + 3, 47, 22, 7, '#e4f4ff', {class: 'window'});
      path(face, 'M' + (wx + 14) + ' 46v9', {stroke: '#fff', 'stroke-width': 2});
    }
    if (node.resource) {
      rect(group, 28, 31, 59, 16, '#ffe18a', {rx: 4, ...line});
      text(group, shorten(node.resource, 9), 57, 43, 9, {'text-anchor': 'middle'});
    }
    const heroFacing = svg('g', {transform: facade + '131 42)'}, group);
    drawHero(heroFacing, heroFor(node, kind), 0, 0, 46, 49);
    drawBuildingStatus(group, node);
  }

  function drawBuilding(group, node) {
    const kind = Object.hasOwn(roles, node.kind) ? node.kind : 'code';
    const [paint, trim, icon] = roles[kind];
    const line = {class: 'comic-stroke'};
    // Pavement and bold offset shadows keep the silhouette clear against the town.
    rect(group, 1, 127, 199, 17, '#172033', {rx: 6});
    rect(group, 1, 123, 196, 17, '#ffefcf', {rx: 6, ...line});
    path(group, 'M6 134h188', {fill: 'none', stroke: '#d09c75', 'stroke-width': 1.5});
    rect(group, 13, 34, 151, 98, '#172033', {rx: 5});
    rect(group, 9, 30, 151, 98, paint, {rx: 5, ...line});
    path(group, 'M9 37 20 20h128l13 17Z', {fill: trim, ...line});
    const strip = layoutMode === 'strip';
    if (strip) {
      rect(group, 19, 39, 128, 25, '#fff6e4', {rx: 5, ...line});
      text(group, icon, 30, 57, 15, {'text-anchor': 'middle'});
      text(group, shorten(node.title || node.id, 14), 42, 56, 13);
    } else {
      drawBuildingName(group, node.title || node.id, 16, 39, 137);
    }
    const sill = strip ? 71 : 78;
    for (const x of [22, 63, 104]) {
      rect(group, x, sill, 31, 90 - sill, '#fff', {rx: 3, ...line});
      rect(group, x + 3, sill + 3, 25, 84 - sill, '#e4f4ff', {rx: 1.5, class: 'window'});
      path(group, 'M' + (x + 15) + ' ' + (sill + 2) + 'V88', {stroke: '#fff', 'stroke-width': 2});
    }
    path(group, 'M19 91h129l9 16H10Z', {fill: '#fff6e4', ...line});
    for (const x of [19, 50, 81, 112]) {
      path(group, 'M' + x + ' 92h15l-3 14h-19Z', {fill: trim});
    }
    path(group, 'M10 107h147', {stroke: '#172033', 'stroke-width': 2.5});
    rect(group, 56, 108, 43, 20, '#465671', {rx: 2, ...line});
    path(group, 'M78 110v16', {stroke: '#fff4e0', 'stroke-width': 2});
    if (node.resource) {
      path(group, 'M16 19q12 1 21-4 9 5 21 4v15q-12 6-21 7-9-1-21-7Z',
        {fill: '#ffe18a', ...line});
      text(group, shorten(node.resource, 7), 37, 31, 8, {'text-anchor': 'middle'});
    }
    const heroId = heroFor(node, kind);
    drawHero(group, heroId, heroId === 'gate' ? 142 : 132, heroId === 'gate' ? 64 : -2, 59, 61);
    drawBuildingStatus(group, node);
  }

  function drawBuildingStatus(group, node) {
    const status = buildingStatus(node);
    path(group, 'M104 109h83l-4 5 11 4-9 5 4 6h-84l3-5-8-4 7-5Z',
      {fill: 'white', class: 'comic-stroke status-fill'});
    text(group, status, 147, 122, 8.7, {'text-anchor': 'middle',
      ...(status.length > 13 ? {textLength: 70, lengthAdjust: 'spacingAndGlyphs'} : {})});
    const attempts = node.attempts || 0;
    rect(group, 17, 134, 115, 16, '#fff', {rx: 6, class: 'comic-stroke'});
    text(group, seconds(node.duration_s) + '  •  ' + attempts + (attempts === 1 ? ' attempt' : ' attempts'), 74, 145, 10,
      {'text-anchor': 'middle'});
    const progress = node.sub_progress;
    if (progress?.total > 0) {
      const ratio = progress.done / progress.total;
      const mark = ratio === 1 ? ' ✓' : progress.running > 0 ? ' ⚡' : '';
      rect(group, 15, 108, 81, 20, '#fff6e4', {rx: 5, class: 'comic-stroke'});
      text(group, progress.done + '/' + progress.total + mark + ' ↗', 55, 119, 9.5, {'text-anchor': 'middle'});
      rect(group, 22, 122, 67, 3, '#dccdb4', {rx: 1.5});
      if (ratio > 0) rect(group, 22, 122, 67 * ratio, 3, '#2f915b', {rx: 1.5});
    }
  }

  function drawBeacons() {
    if (!spotlights.size) return;
    const layer = svg('g', {class: 'beacons', 'aria-hidden': 'true'}, world);
    const toWorld = world.getCTM().inverse();
    const iso = layoutMode === 'iso';
    for (const [taskPath, kind] of spotlights) {
      const entry = targets.get(taskPath);
      if (!entry) continue;
      const {target, node} = entry;
      const transform = toWorld.multiply(target.getCTM());
      const {a, b, c, d, e, f} = transform;
      const marker = svg('g', {class: statusClass(node.status)}, layer);
      const halo = svg('g', {class: 'beacon-halo', transform: 'matrix(' + [a, b, c, d, e, f].join(' ') + ')'}, marker);
      const y = iso ? 124 : 129;
      rect(halo, 2, y, 197, 151 - y, 'none', {rx: 8, class: 'beacon-outline'});
      rect(halo, 2, y, 197, 151 - y, 'none', {rx: 8, class: 'beacon-light'});

      // Project the roof anchor and keep the pin and its lettering upright.
      const roof = iso ? point(94, 102, 68) : [84, 20];
      const anchor = new DOMPoint(...roof).matrixTransform(transform);
      const placement = svg('g', {transform: 'translate(' + anchor.x + ' ' + anchor.y + ')'}, marker);
      const pin = svg('g', {class: 'beacon-pin'}, placement);
      path(pin, 'M-43-35H43Q49-35 49-29V-16Q49-10 43-10H8L0-2-8-10H-43Q-49-10-49-16V-29Q-49-35-43-35Z',
        {class: 'comic-stroke'});
      text(pin, kind.pin, 0, -18, 12, {'text-anchor': 'middle', 'letter-spacing': .8});
    }
  }

  function applyCamera() {
    world.setAttribute('transform', 'translate(' + camera.x + ' ' + camera.y + ') scale(' + camera.scale + ')');
  }
  function toolbarInset() {
    const inset = controls.offsetTop + controls.offsetHeight + 10;
    stage.style.setProperty('--inspector-top', inset + 'px');
    return inset;
  }
  function viewportSpace(wrapped) {
    const top = toolbarInset() + 8;
    const bottom = layoutMode === 'iso' || wrapped ? 24 : 60;
    const {clientWidth, clientHeight} = viewport;
    if (!clientWidth || !clientHeight) {
      const aspect = document.body.classList.contains('full') ? 1.8 : 1.45;
      return {w: aspect, h: 1, aspect, top, bottom};
    }
    const w = Math.max(1, clientWidth - 2 * mapInsetX);
    const h = Math.max(1, clientHeight - top - bottom);
    return {w, h, aspect: w / h, top, bottom};
  }
  function measureTownSpace() {
    const wrapped = stage.hasAttribute('data-wrapped');
    const current = viewportSpace(wrapped);
    if (layoutMode === 'strip') return {single: current, wrapped: current};
    // Wrapped embeds have a different CSS stage height. Score both layouts
    // against the space they will have once rendered.
    stage.toggleAttribute('data-wrapped', !wrapped);
    const alternate = viewportSpace(!wrapped);
    stage.toggleAttribute('data-wrapped', wrapped);
    return wrapped ? {single: alternate, wrapped: current} : {single: current, wrapped: alternate};
  }
  function fit() {
    cancelAnimationFrame(cameraFrame);
    const space = viewportSpace(stage.hasAttribute('data-wrapped'));
    if (!targets.size) return;
    const {clientWidth: w, clientHeight: h} = viewport;
    const bounds = world.getBBox();
    camera.scale = Math.min(1.5, space.w / Math.max(1, bounds.width), space.h / Math.max(1, bounds.height));
    camera.x = w / 2 - (bounds.x + bounds.width / 2) * camera.scale;
    camera.y = (space.top + h - space.bottom) / 2 - (bounds.y + bounds.height / 2) * camera.scale;
    applyCamera();
  }
  function focusNode(path, {scroll = true} = {}) {
    if (!targets.has(path) && path?.includes('::')) {
      const parent = nodeIndex.get(path.split('::', 1)[0]);
      if (parent && nodeIndex.has(path)) openSubGraph(parent, path, {scroll});
      return;
    }
    if (!targets.has(path) && drillStack.length && nodeIndex.has(path)) drillTo([]);
    const entry = targets.get(path);
    if (!entry) return;
    select(path, {located: true});
    cameraMoved = true;
    cancelAnimationFrame(cameraFrame);

    const toWorld = world.getCTM().inverse().multiply(entry.target.getCTM());
    const center = new DOMPoint(entry.w / 2, entry.h / 2).matrixTransform(toWorld);
    const {clientWidth: w, clientHeight: h} = viewport;
    const sheet = !docked() && inspector.offsetLeft === 0;
    const right = docked() || sheet ? w : inspector.offsetLeft - 8;
    const bottom = sheet ? inspector.offsetTop - 8 : h;
    const scale = sheet ? .95 : 1.1;
    const destination = {x: right / 2 - center.x * scale,
      y: (toolbarInset() + bottom) / 2 - center.y * scale, scale};
    if (scroll && !docked()) {
      stage.scrollIntoView({block: 'nearest', behavior: reducedMotion.matches ? 'auto' : 'smooth'});
    }
    if (reducedMotion.matches) {
      Object.assign(camera, destination);
      applyCamera();
      return;
    }

    const start = {...camera};
    const started = performance.now();
    function move(now) {
      const time = Math.min(1, Math.max(0, (now - started) / 360));
      const progress = 1 - (1 - time) ** 3;
      for (const key of ['x', 'y', 'scale']) camera[key] = start[key] + (destination[key] - start[key]) * progress;
      applyCamera();
      if (time < 1) cameraFrame = requestAnimationFrame(move);
    }
    cameraFrame = requestAnimationFrame(move);
  }
  function zoomAt(ratio, x = viewport.clientWidth / 2, y = viewport.clientHeight / 2, toX = x, toY = y) {
    cancelAnimationFrame(cameraFrame);
    cameraMoved = true;
    const next = Math.min(5, Math.max(Math.min(.06, camera.scale), camera.scale * ratio));
    camera.x = toX - (x - camera.x) * next / camera.scale;
    camera.y = toY - (y - camera.y) * next / camera.scale;
    camera.scale = next;
    applyCamera();
  }
  function pinch() {
    const [first, second] = pointers.values();
    if (!second) return null;
    const bounds = viewport.getBoundingClientRect();
    return {
      x: (first.x + second.x) / 2 - bounds.left,
      y: (first.y + second.y) / 2 - bounds.top,
      distance: Math.max(1, Math.hypot(first.x - second.x, first.y - second.y))
    };
  }
  function startDrag() {
    cancelAnimationFrame(cameraFrame);
    gestureDragged = true;
    cameraMoved = true;
    lastSkyTap = null;
    viewport.classList.add('dragging');
    for (const id of pointers.keys()) viewport.setPointerCapture(id);
  }
  viewport.addEventListener('pointerdown', event => {
    if (event.button !== 0 || (pointers.size && event.pointerType !== 'touch')) return;
    if (!pointers.size) gestureDragged = false;
    pointers.set(event.pointerId, {
      x: event.clientX, y: event.clientY, startX: event.clientX, startY: event.clientY,
      sky: !event.target.closest('.node-target')
    });
    if (pointers.size > 1) startDrag();
  });
  viewport.addEventListener('pointermove', event => {
    const pointer = pointers.get(event.pointerId);
    if (!pointer) return;
    const before = pinch();
    const dx = event.clientX - (gestureDragged ? pointer.x : pointer.startX);
    const dy = event.clientY - (gestureDragged ? pointer.y : pointer.startY);
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    if (before) {
      const after = pinch();
      zoomAt(after.distance / before.distance, before.x, before.y, after.x, after.y);
    } else {
      if (!gestureDragged && Math.hypot(dx, dy) < 5) return;
      if (!gestureDragged) startDrag();
      camera.x += dx;
      camera.y += dy;
      applyCamera();
    }
  });
  function endPointer(event) {
    const pointer = pointers.get(event.pointerId);
    if (!pointer) return;
    pointers.delete(event.pointerId);
    if (pointers.size) return;
    viewport.classList.remove('dragging');
    if (event.type === 'pointerup' && event.pointerType === 'touch' && !gestureDragged && pointer.sky) {
      const tap = {time: performance.now(), x: event.clientX, y: event.clientY};
      if (lastSkyTap && tap.time - lastSkyTap.time < 350 &&
          Math.hypot(tap.x - lastSkyTap.x, tap.y - lastSkyTap.y) < 32) {
        fitTown();
        lastSkyTap = null;
      } else {
        lastSkyTap = tap;
      }
    } else {
      lastSkyTap = null;
    }
  }
  viewport.addEventListener('pointerup', endPointer);
  viewport.addEventListener('pointercancel', endPointer);
  viewport.addEventListener('pointerleave', event => {
    if (!viewport.hasPointerCapture(event.pointerId)) endPointer(event);
  });
  viewport.addEventListener('lostpointercapture', event => {
    if (event.target === viewport) endPointer(event);
  });
  viewport.addEventListener('wheel', event => {
    if (!event.deltaY) return;
    event.preventDefault();
    const bounds = viewport.getBoundingClientRect();
    zoomAt(event.deltaY < 0 ? 1.14 : 1 / 1.14, event.clientX - bounds.left, event.clientY - bounds.top);
  }, {passive: false});
  $('zoom-in').addEventListener('click', () => zoomAt(1.25));
  $('zoom-out').addEventListener('click', () => zoomAt(.8));
  function fitTown() {
    cameraMoved = false;
    fit();
  }
  $('fit-town').addEventListener('click', fitTown);
  window.addEventListener('resize', () => {
    // Reflow on aspect changes large enough to affect the street layout.
    const {aspect} = viewportSpace(stage.hasAttribute('data-wrapped'));
    if (layoutMode !== 'strip' && Math.abs(Math.log(aspect / layoutSpace.aspect)) > .14) {
      const previousSpace = layoutSpace;
      const entry = cameraMoved && targets.get(selected);
      const anchor = entry && new DOMPoint(entry.w / 2, entry.h / 2);
      const previous = anchor && anchor.matrixTransform(entry.target.getCTM());
      redrawTown();
      if (previous) {
        const current = anchor.matrixTransform(targets.get(selected).target.getCTM());
        const x = mapInsetX + (previous.x - mapInsetX) * layoutSpace.w / previousSpace.w;
        const y = layoutSpace.top + (previous.y - previousSpace.top) * layoutSpace.h / previousSpace.h;
        cancelAnimationFrame(cameraFrame);
        camera.x += x - current.x;
        camera.y += y - current.y;
        applyCamera();
      }
    }
    if (!cameraMoved) fit();
  });
  for (const button of layoutButtons) {
    button.addEventListener('click', () => {
      if (layoutMode !== button.dataset.layout) {
        layoutMode = button.dataset.layout;
        for (const choice of layoutButtons) {
          choice.setAttribute('aria-pressed', String(choice === button));
        }
        redrawTown();
      }
      fitTown();
    });
  }
  $('theme').addEventListener('click', event => {
    const night = document.body.dataset.theme === 'day';
    document.body.dataset.theme = night ? 'night' : 'day';
    event.currentTarget.setAttribute('aria-pressed', String(night));
    event.currentTarget.textContent = night ? '☀ Daytime' : '☾ Night Patrol';
  });

  function element(tag, value) {
    const el = document.createElement(tag);
    el.textContent = value;
    return el;
  }
  function hoverTask(path) {
    targets.get(hoveredTask)?.target.classList.remove('hovered');
    hoveredTask = path;
    targets.get(path)?.target.classList.add('hovered');
  }
  function createRosterRow(path) {
    const row = document.createElement('li');
    const button = element('button', '');
    button.type = 'button';
    button.className = 'roster-row';
    for (const field of ['step', 'status', 'name', 'spotlight', 'timing', 'detail']) {
      const span = element('span', '');
      span.className = 'roster-' + field;
      button.append(span);
    }
    const [, status, name] = button.children;
    const badge = element('span', '');
    badge.className = 'roster-badge';
    status.append(badge);
    const breadcrumb = element('span', '');
    breadcrumb.className = 'roster-breadcrumb';
    name.append(breadcrumb, element('strong', ''));
    button.addEventListener('click', () => focusNode(path));
    button.addEventListener('pointerenter', event => { if (event.pointerType !== 'touch') hoverTask(path); });
    button.addEventListener('pointerleave', () => { if (hoveredTask === path) hoverTask(null); });
    row.append(button);
    return row;
  }
  function applyRosterFilter() {
    let visible = 0;
    for (const [path, row] of rosterRows) {
      const group = row.dataset.group;
      row.hidden = rosterFilter !== 'all' && (rosterFilter === 'active' ?
        group !== 'running' && group !== 'waiting' : group !== rosterFilter);
      if (!row.hidden) visible++;
      else if (hoveredTask === path) hoverTask(null);
    }
    rosterList.hidden = !visible;
    const empty = $('roster-empty');
    empty.hidden = Boolean(visible);
    empty.textContent = tasks.length ? 'No tasks match this filter.' : 'No tasks to display.';
  }
  function setRosterFilter(filter) {
    rosterFilter = filter;
    for (const button of rosterFilters.children) button.setAttribute('aria-pressed', String(button.dataset.filter === filter));
    applyRosterFilter();
  }
  rosterFilters.addEventListener('click', event => {
    const filter = event.target.closest('button')?.dataset.filter;
    if (filter) setRosterFilter(filter);
  });

  function renderRoster() {
    const paths = new Set(tasks.map(task => task.path));
    for (const [path, row] of rosterRows) {
      if (paths.has(path)) continue;
      if (hoveredTask === path) hoverTask(null);
      row.remove();
      rosterRows.delete(path);
    }
    tasks.forEach(({node, path, parents}, i) => {
      const row = rosterRows.get(path) || createRosterRow(path);
      rosterRows.set(path, row);
      if (rosterList.children[i] !== row) rosterList.insertBefore(row, rosterList.children[i] || null);
      row.dataset.group = taskGroup(node.status);
      const button = row.firstElementChild;
      if (path === selected) button.setAttribute('aria-current', 'true');
      const [step, status, name, pill, timing, subtitle] = button.children;
      const [breadcrumb, id] = name.children;
      const color = statusClass(node.status);
      step.textContent = '#' + (i + 1);
      status.className = 'roster-status ' + color;
      const label = statusLabel(node.status);
      status.title = label;
      status.lastElementChild.textContent = {'Awaiting subagents': 'Paused', 'Circuit broken': 'Failed',
        'Budget exceeded': 'Budget'}[label] || label;
      breadcrumb.textContent = parents.length ? parents.map(titleCase).join(' › ') + ' › ' : '';
      id.textContent = titleCase(node.id);
      name.title = breadcrumb.textContent + id.textContent;
      const kind = frontierKinds.find(kind => kind.group !== 'pending' && kind.group === row.dataset.group);
      pill.hidden = !kind;
      pill.className = 'roster-spotlight ' + color;
      pill.textContent = kind?.pill || '';
      timing.textContent = (node.attempts ?? 0) + '× · ' + seconds(node.duration_s);
      const context = attention?.node_path === path ? attention : null;
      const taskDetail = node.detail || node.error_signature || '';
      const mission = context?.headline !== id.textContent ? context?.headline : null;
      subtitle.textContent = [mission, context?.detail || taskDetail].filter(Boolean).join(' · ');
      subtitle.title = subtitle.textContent;
      subtitle.hidden = !subtitle.textContent;
    });
    applyRosterFilter();
  }

  function revealRosterRow(path, current = true) {
    const row = rosterRows.get(path);
    if (!row) return;
    if (current) row.firstElementChild.setAttribute('aria-current', 'true');
    if (roster.hidden || row.hidden) return;
    const list = rosterList.getBoundingClientRect();
    const bounds = row.getBoundingClientRect();
    const inset = 6;
    const offset = bounds.top < list.top + inset ? bounds.top - list.top - inset :
      Math.max(0, bounds.bottom - list.bottom + inset);
    if (offset) rosterList.scrollBy({top: offset, behavior: reducedMotion.matches ? 'auto' : 'smooth'});
  }

  function shellQuote(value) {
    return /^[\w@%+=:,./-]+$/.test(value) && !value.startsWith('-') ?
      value : "'" + String(value).replace(/'/g, "'\"'\"'") + "'";
  }
  function actionCommands(node) {
    if (!snapshot.workflow_ref || node.path.includes('::')) return null;
    // The CLI can retry splitter descendants. Checkpointing stops at the outer splitter.
    const parts = node.path.split('/');
    let checkpoint = node.path;
    for (let n = 1; n < parts.length; n++) {
      const parent = parts.slice(0, n).join('/');
      if (targets.get(parent)?.node.node_type === 'splitter') {
        checkpoint = parent;
        break;
      }
    }
    let base = 'kgraph-town run ' + shellQuote(snapshot.workflow_ref);
    if (snapshot.project_dir) {
      base = 'cd -- ' + shellQuote(snapshot.project_dir) + ' && ' + base;
      for (const [flag, value] of [['--state', snapshot.state_file], ['--report', snapshot.report_file],
        ['--factory', snapshot.factory === 'build_graph' ? null : snapshot.factory]]) {
        if (value) base += ' ' + flag + ' ' + shellQuote(value);
      }
    }
    return {
      retry: base + ' --retry ' + shellQuote(node.path),
      until: base + ' --until ' + shellQuote(checkpoint), checkpoint
    };
  }
  function select(value, {located = false} = {}) {
    const restoreFocus = docked() && roster.contains(document.activeElement);
    targets.get(selected)?.target.classList.remove('selected');
    rosterRows.get(selected)?.firstElementChild.removeAttribute('aria-current');
    if (value !== selected) clearLog();
    selected = value;
    stage.toggleAttribute('data-located', located);
    targets.get(selected)?.target.classList.add('selected');
    drawInspector();
    revealRosterRow(selected);
    if (restoreFocus && !inspector.hidden) $('close-inspector').focus({preventScroll: true});
  }
  function drawInspector() {
    const node = targets.get(selected)?.node;
    syncInspector();
    if (!node) return;
    $('inspect-title').textContent = node.title || titleCase(node.id);
    const fields = $('inspect-fields');
    fields.replaceChildren();
    const facts = [
      ['Path', node.path], ['Status', statusLabel(node.status)], ['Kind', titleCase(node.kind)],
      ['Attempts', node.attempts ?? 0], ['Duration', seconds(node.duration_s)],
      ['Resource lock', node.resource], ['Input hash', node.input_hash],
      ['Error signature', node.error_signature], ['Detail', node.detail],
      ['Started', node.started_at], ['Updated', node.updated_at]
    ];
    if (node.cache_miss_reason) facts.push(['Cache miss', node.cache_miss_reason]);
    if (node.path.includes('::')) {
      facts.push(['Agent', node.agent], ['Progress', node.progress == null ? null : node.progress + '%']);
    }
    if (attention?.node_path === selected) facts.unshift(['Mission', attention.headline]);
    $('inspect-input').hidden = attention?.input_gate?.node_path !== selected &&
      (!node.input_gate || Boolean(node.input_gate.submitted));
    renderInspectorSubtasks(node);
    const parent = nodeIndex.get(node.path.slice(0, node.path.lastIndexOf('/')));
    const stageTitle = path => titleCase(nodeIndex.get(path)?.id ?? path.split('/').at(-1));
    for (const edge of parent?.edges || []) {
      if (edge.kind === 'retry' && edge.from === node.path) {
        facts.push(['On failure', 'Routes back to ' + stageTitle(edge.to) + ' (' + edge.label + ')']);
      }
    }
    if (node.node_type === 'loop_subgraph') {
      for (const edge of node.edges.filter(edge => edge.kind === 'retry')) {
        const limit = node.max_retries == null ? '' : ' (max ' + node.max_retries + ')';
        facts.push(['Retry loop', stageTitle(edge.from) + ' → ' + stageTitle(edge.to) + limit]);
      }
    }
    for (const [name, value] of facts) fields.append(element('dt', name), element('dd', value ?? '—'));
    const outputs = $('inspect-outputs');
    const focusedOutput = outputs.contains(document.activeElement) ? document.activeElement.dataset.output : undefined;
    outputs.replaceChildren();
    const digests = node.output_hashes || {};
    const names = new Set([...(node.outputs || []), ...Object.keys(digests)]);
    for (const name of names) {
      const line = element('li', name);
      if (options.logs) {
        const button = element('button', 'View');
        Object.assign(button, {type: 'button', className: 'inspect-log-view'});
        button.dataset.output = name;
        button.setAttribute('aria-label', 'View ' + name);
        button.addEventListener('click', () => viewLog(name));
        line.append(button);
      }
      if (Object.hasOwn(digests, name)) line.append(document.createElement('br'), element('code', digests[name]));
      outputs.append(line);
    }
    if (focusedOutput !== undefined) focusLogOutput(focusedOutput);
    if (logPath !== undefined && !names.has(logPath)) clearLog();
    if (!names.size) outputs.append(element('li', 'No recorded outputs'));
    const commands = actionCommands(node);
    $('copy-retry').disabled = $('copy-until').disabled = !commands;
    $('copy-until').textContent = commands && commands.checkpoint !== node.path ? 'Copy until splitter' : 'Copy until';
    $('copy-note').textContent = !commands ? (node.path.includes('::') ? '' : 'A workflow path is required to run commands.') :
      (commands.checkpoint === node.path ? '' : 'Checkpoint: ' + commands.checkpoint);
    $('copy-result').textContent = '';
    $('copy-fallback').hidden = true;
  }
  function runEndpoint(base, suffix) {
    if (!options.runs) return base;
    return displayedRunId ? base + '/' + encodeURIComponent(displayedRunId) + '/' + suffix : null;
  }
  function focusLogOutput(path, preventScroll = true) {
    [...$('inspect-outputs').querySelectorAll('[data-output]')]
      .find(button => button.dataset.output === path)?.focus({preventScroll});
  }
  function clearLog() {
    logRequest?.abort();
    logRequest = logPath = undefined;
    $('inspect-log').hidden = true;
    $('inspect-log-title').textContent = '';
    $('inspect-log-status').textContent = '';
    $('inspect-log-content').textContent = '';
    $('inspect-log-content').hidden = true;
  }
  async function viewLog(path) {
    const endpoint = runEndpoint(options.logs, 'logs');
    if (!endpoint || (options.runs && activeRunId !== displayedRunId)) return;
    clearLog();
    const request = new AbortController();
    logRequest = request;
    logPath = path;
    const section = $('inspect-log');
    const status = $('inspect-log-status');
    const content = $('inspect-log-content');
    $('inspect-log-title').textContent = path.split(/[\\/]/).pop() || path;
    status.textContent = 'Loading output…';
    section.hidden = false;
    $('close-log').focus();
    try {
      const response = await fetch(endpoint + '?path=' + encodeURIComponent(path),
        {cache: 'no-store', signal: request.signal});
      const data = await response.json();
      if (!response.ok) throw new Error(data.error || 'HTTP ' + response.status);
      if (logRequest !== request) return;
      $('inspect-log-title').textContent = data.name;
      status.textContent = data.truncated ? 'Showing the end of the file (truncated).' : '';
      content.textContent = data.content;
      content.hidden = false;
    } catch (error) {
      if (logRequest === request) status.textContent = 'Could not load output. ' + error.message;
    }
  }
  $('close-log').addEventListener('click', () => {
    const path = logPath;
    clearLog();
    focusLogOutput(path, false);
  });
  function renderInspectorSubtasks(node) {
    const section = $('inspect-subtasks');
    const openButton = $('open-subgraph');
    const graph = subGraph(node);
    section.hidden = !graph;
    openButton.hidden = !graph;
    if (!graph) {
      openButton.onclick = null;
      return;
    }
    const progress = subProgress(node, graph);
    openButton.textContent = 'Open Sub-graph ↗';
    $('inspect-subtasks-title').textContent = 'Sub-tasks (' + progress.done + '/' + progress.total + ')';
    const bar = $('inspect-subtask-progress');
    bar.max = progress.total;
    bar.value = progress.done;
    const list = $('inspect-subtask-list');
    const scroll = list.scrollTop;
    const focused = list.contains(document.activeElement) && document.activeElement.dataset.path;
    list.replaceChildren(...graph.nodes.map(child => {
      const item = element('li', '');
      const button = element('button', '');
      button.type = 'button';
      button.dataset.path = child.path;
      const dot = element('span', '');
      dot.className = 'dot ' + statusClass(child.status);
      dot.title = statusLabel(child.status);
      dot.setAttribute('aria-label', dot.title);
      const label = element('span', '');
      label.append(element('strong', child.title || titleCase(child.id)));
      if (child.detail) label.append(element('small', child.detail));
      if (child.agent) {
        const agent = element('small', child.agent);
        agent.className = 'gate-pill';
        label.append(agent);
      }
      button.append(dot, label);
      button.addEventListener('click', () => openSubGraph(node, child.path));
      item.append(button);
      return item;
    }));
    list.scrollTop = scroll;
    if (focused !== false) [...list.querySelectorAll('button')].find(button => button.dataset.path === focused)?.focus();
    openButton.onclick = () => openSubGraph(node);
  }

  async function copyValue(value, field, status, button, label) {
    try {
      await navigator.clipboard.writeText(value);
      field.hidden = true;
      status.textContent = 'Copied';
    } catch {
      field.value = value;
      field.hidden = false;
      field.focus();
      field.select();
      if (document.execCommand?.('copy')) {
        field.hidden = true;
        status.textContent = 'Copied';
        button.focus();
      } else {
        status.textContent = label + ' selected for copying';
      }
    }
  }
  function copyCommand(key) {
    const commands = actionCommands(targets.get(selected).node);
    return copyValue(commands[key], $('copy-fallback'), $('copy-result'), $('copy-' + key), 'Command');
  }
  function closeInspector() {
    const path = selected;
    const target = targets.get(path)?.target;
    const row = rosterRows.get(path);
    select(null);
    if (docked()) {
      revealRosterRow(path, false);
      const control = row && !row.hidden ? row.firstElementChild : rosterFilters.querySelector('[aria-pressed=true]');
      control?.focus({preventScroll: true});
    } else {
      target?.focus({preventScroll: true});
    }
  }
  $('close-inspector').addEventListener('click', closeInspector);
  $('inspect-input').addEventListener('click', () => {
    if (attention?.input_gate?.node_path !== selected) {
      preferredInputPath = selected;
      renderAttention();
    }
    const form = $('input-gate');
    form.scrollIntoView({block: 'nearest'});
    form.focus({preventScroll: true});
  });
  document.addEventListener('keydown', event => {
    if (event.key !== 'Escape' || event.defaultPrevented || $('qa-panel').contains(event.target)) return;
    if ($('inspect-log').contains(event.target) && !$('inspect-log').hidden) $('close-log').click();
    else if (selected !== null) closeInspector();
    else if (drillStack.length) drillTo(drillStack.slice(0, -1));
  });
  $('copy-retry').addEventListener('click', () => copyCommand('retry'));
  $('copy-until').addEventListener('click', () => copyCommand('until'));
  $('device-list')?.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    const url = button.closest('li').querySelector('a').href;
    copyValue(url, $('device-fallback'), $('device-copy-result'), button, 'Address');
  });

  function renderHud() {
    $('workflow-title').textContent = snapshot.title;
    document.title = snapshot.title + ' | Hero Town';
    const status = $('overall');
    status.className = 'overall ' + statusClass(snapshot.status);
    status.textContent = statusLabel(snapshot.status);
    const counts = snapshot.counts || {};
    $('count-passed').textContent = counts.passed || 0;
    $('count-cached').textContent = counts.cached || 0;
    $('count-running').textContent = counts.running || 0;
    $('count-failed').textContent = (counts.failed || 0) + (counts.circuit_broken || 0) + (counts.budget_exceeded || 0);
    $('count-pending').textContent = (counts.pending || 0) + (counts.awaiting_subagents || 0);
    const {worker_calls_used: used, max_worker_calls: max} = snapshot.summary || {};
    $('budget-label').textContent = (used ?? '—') + ' / ' + (max ?? (used == null ? '—' : '∞'));
    const bar = $('budget-meter');
    bar.max = Math.max(1, max ?? 1);
    bar.value = used ?? 0;
    bar.hidden = max == null;
    $('warning').textContent = snapshot.warning || '';
    $('warning').hidden = !snapshot.warning;
  }
  function renderReport() {
    const rows = {
      'slowest-nodes': (snapshot.slowest_nodes || []).map(row => row.node + ' · ' + seconds(row.duration_s) + ' · ' +
        row.attempts + (row.attempts === 1 ? ' attempt' : ' attempts')),
      'failure-signatures': (snapshot.failure_signatures || []).map(row => row.node + ' · ' +
        (row.error_signature || statusLabel(row.status)) + (row.detail ? ' · ' + row.detail : '')),
      'evolution-hints': snapshot.evolution_hints || []
    };
    for (const [id, entries] of Object.entries(rows)) {
      const list = $(id);
      list.replaceChildren(...entries.map(value => element('li', value)));
      if (!entries.length) {
        const empty = element('li', 'None recorded');
        empty.className = 'no-data';
        list.append(empty);
      }
    }
  }
  function editingQa(view) {
    return view.saving || view.drafts.size || $('qa-cases').contains(document.activeElement);
  }
  async function refreshQa() {
    if (!options.qa) return;
    clearTimeout(qaTimer);
    // SSE may stay quiet while a checklist is created or updated on disk.
    if (automatic) qaTimer = setTimeout(refreshQa, 5000);
    const endpoint = runEndpoint(options.qa, 'qa');
    const panel = $('qa-panel');
    if (qaView?.endpoint !== endpoint) {
      qaView?.request?.abort();
      qaView = undefined;
      panel.hidden = true;
      $('qa-cases').replaceChildren();
      const status = $('qa-status');
      status.textContent = '';
      status.removeAttribute('data-error');
    }
    if (!endpoint) return;
    const view = qaView ??= {endpoint, drafts: new Map(), saving: false};
    if (view.request || editingQa(view)) return;
    const request = new AbortController();
    view.request = request;
    try {
      const response = await fetch(endpoint, {cache: 'no-store', signal: request.signal});
      if (!response.ok) return;
      const data = await response.json();
      if (qaView !== view || editingQa(view)) return;
      panel.hidden = !data.available;
      const signature = JSON.stringify(data);
      if (signature === view.signature) return;
      view.signature = signature;
      if (!data.available) return;
      view.canEdit = data.can_edit;
      view.manualValues = data.manual_qa_values;
      view.gapTypes = data.gap_types;
      renderQa(view, data.scorecard);
    } catch {
      // A transient error leaves any displayed scorecard intact.
    } finally {
      view.request = undefined;
    }
  }
  function qaSelect(name, label, choices, selected) {
    const select = element('select', '');
    select.name = name;
    for (const value of choices) {
      const option = element('option', value ? titleCase(value) : 'None');
      option.value = value ?? '';
      select.append(option);
    }
    select.value = selected ?? '';
    const field = element('label', label);
    field.className = 'gate-text';
    field.append(select);
    return {field, select};
  }
  function qaForm(view, qaCase) {
    const form = element('form', '');
    form.noValidate = true;
    form.dataset.caseId = qaCase.id;
    form.setAttribute('aria-label', 'Edit QA result for ' + qaCase.id);
    const values = {...qaCase, ...view.drafts.get(qaCase.id)};
    const manual = qaSelect('manual_qa', 'Manual QA', view.manualValues, values.manual_qa);
    const gap = qaSelect('gap_type', 'Gap type', [null, ...view.gapTypes.filter(Boolean)], values.gap_type);
    const notes = gateText('Notes', 'textarea');
    notes.input.name = 'notes';
    notes.input.value = values.notes ?? '';
    const save = element('button', 'Save');
    Object.assign(save, {type: 'submit', name: 'save'});
    const fields = element('fieldset', '');
    fields.append(manual.field, gap.field, notes.row, save);
    form.append(fields);
    form.addEventListener('input', event => {
      const {name, value} = event.target;
      const draft = view.drafts.get(qaCase.id) || {};
      draft[name] = value;
      view.drafts.set(qaCase.id, draft);
    });
    form.addEventListener('submit', async event => {
      event.preventDefault();
      if (view.saving || (options.runs && activeRunId !== displayedRunId)) return;
      view.saving = true;
      const status = $('qa-status');
      const hadFocus = form.contains(document.activeElement);
      fields.disabled = true;
      status.removeAttribute('data-error');
      status.textContent = 'Saving ' + qaCase.id + '…';
      const payload = {case_id: qaCase.id, manual_qa: manual.select.value,
        gap_type: gap.select.value || null, notes: notes.input.value};
      try {
        const response = await fetch(view.endpoint, {method: 'POST', headers: {'Content-Type': 'application/json'},
          body: JSON.stringify(payload)});
        const data = await response.json();
        if (!response.ok || !data.ok) throw new Error(data.error || 'HTTP ' + response.status);
        if (qaView !== view) return;
        const focused = document.activeElement;
        const focusCase = focused.form?.dataset.caseId ?? (hadFocus && focused === document.body ? qaCase.id : null);
        view.drafts.delete(qaCase.id);
        view.canEdit = data.can_edit;
        renderQa(view, data.scorecard);
        if (focusCase != null) {
          const next = [...$('qa-cases').querySelectorAll('form')].find(row => row.dataset.caseId === focusCase);
          (next?.elements.namedItem(focused.name || 'save') || $('qa-panel').firstElementChild).focus({preventScroll: true});
        }
        status.textContent = 'Saved ' + qaCase.id + '.';
      } catch (error) {
        if (qaView !== view) return;
        status.dataset.error = '';
        status.textContent = 'Could not save ' + qaCase.id + '. ' + error.message;
      } finally {
        view.saving = false;
        fields.disabled = false;
        if (form.isConnected && hadFocus && document.activeElement === document.body) save.focus({preventScroll: true});
      }
    });
    return form;
  }
  function renderQa(view, scorecard) {
    const {counts, rates, cases} = scorecard;
    const percent = value => value == null ? '—' : value + '%';
    const kpis = [
      'Pass % ' + percent(rates.manual_pass_pct), 'One-Shot % ' + percent(rates.one_shot_pct),
      '≤2-Shot % ' + percent(rates.within_two_attempts_pct), 'Tested ' + counts.tested + '/' + counts.total
    ];
    $('qa-kpis').replaceChildren(...kpis.map(value => element('span', value)));
    const list = $('qa-cases');
    list.replaceChildren();
    for (const qaCase of cases) {
      const row = element('li', '');
      const heading = element('h3', qaCase.title);
      heading.prepend(element('code', qaCase.id), ' ');
      const meta = [qaCase.platform, 'Automated ' + titleCase(qaCase.automated_status),
        'AI attempts ' + qaCase.ai_attempts, 'Manual QA ' + titleCase(qaCase.manual_qa)];
      row.append(heading, element('p', meta.join(' · ')));
      if (view.canEdit) row.append(qaForm(view, qaCase));
      else if (qaCase.notes) {
        const notes = element('p', qaCase.notes);
        notes.className = 'qa-notes';
        row.append(notes);
      }
      list.append(row);
    }
    if (!cases.length) list.append(element('li', 'No QA cases recorded.'));
  }
  function redrawTown() {
    const focused = [...targets.entries()].find(([, entry]) => entry.target === document.activeElement)?.[0];
    targets.clear();
    world.replaceChildren();
    renderBreadcrumbs();
    townSpace = measureTownSpace();
    const graph = stageGraph();
    const level = measureLevel(graph.nodes, graph.edges, {root: true, parallel: Boolean(graph.parallel)});
    stage.dataset.layout = layoutMode;
    const wrapped = level.rows.length > 1;
    stage.toggleAttribute('data-wrapped', wrapped);
    layoutSpace = wrapped ? townSpace.wrapped : townSpace.single;
    // Project each district and street before measuring so empty corners
    // of the full grid don't count toward the isometric camera bounds.
    const projection = layoutMode === 'iso' ? 'matrix(1 ' + isoTilt + ' ' + isoSkew + ' ' + isoDepth + ' 0 0) ' : '';
    drawLevel(world, level, projection);
    drawBeacons();
    targets.get(selected)?.target.classList.add('selected');
    targets.get(hoveredTask)?.target.classList.add('hovered');
    targets.get(focused)?.target.focus({preventScroll: true});
  }

  function update(next, initial = false) {
    next = {title: 'Hero Town', status: 'pending', ...next,
      nodes: Array.isArray(next.nodes) ? next.nodes : [], edges: Array.isArray(next.edges) ? next.edges : []};
    const previousRef = snapshot?.workflow_ref;
    const previousMission = attention?.kind + ":" + attention?.node_path;
    const previousSelection = targets.get(selected)?.node;
    snapshot = next;
    renderHud();
    renderReport();
    const graph = JSON.stringify([next.nodes, next.edges]);
    const newGraph = graph !== graphSignature;
    const drillDepth = drillStack.length;
    if (newGraph) {
      nodeIndex.clear();
      indexNodes(next.nodes);
      const missing = drillStack.findIndex(path => !subGraph(nodeIndex.get(path)));
      if (missing !== -1) drillStack = drillStack.slice(0, missing);
      updateTaskProgress();
    }
    const newAttention = renderAttention();
    const missionChanged = previousMission !== attention?.kind + ":" + attention?.node_path;
    if (newGraph || missionChanged || initial) {
      graphSignature = graph;
      redrawTown();
      renderRoster();
      if (initial || drillDepth !== drillStack.length || (!fittedNodes && targets.size && !cameraMoved)) fit();
      fittedNodes ||= targets.size > 0;
    } else if (newAttention) {
      renderRoster();
    }
    $('empty').hidden = Boolean(stageGraph().nodes.length);
    $('empty').textContent = options.runs && !displayedRunId ? 'Run a workflow to add it to Hero Town' : 'No stages to display';
    if (selected !== null) {
      const current = targets.get(selected);
      if (!current) select(null);
      else if (newAttention || JSON.stringify(previousSelection) !== JSON.stringify(current?.node) ||
          previousRef !== snapshot.workflow_ref) {
        drawInspector();
      }
    }
  }

  function showLive(status, text) {
    $('live-badge').dataset.live = status;
    $('live-text').textContent = text;
  }
  function liveUrl(endpoint) {
    const url = new URL(endpoint, location.href);
    if (options.runs && activeRunId) url.searchParams.set('run', activeRunId);
    return url;
  }
  function closeEvents() {
    clearTimeout(eventRetryTimer);
    const source = eventStream;
    eventStream = undefined;
    source?.close();
  }
  function retryEvents() {
    clearTimeout(eventRetryTimer);
    if (automatic) eventRetryTimer = setTimeout(connectEvents, 5000);
  }
  function connectEvents() {
    if (!automatic || !options.events || !window.EventSource) return false;
    const requestedRun = activeRunId;
    let source;
    try {
      source = new EventSource(liveUrl(options.events));
    } catch {
      retryEvents();
      return false;
    }
    eventStream = source;
    showLive('offline', 'Connecting');
    function fallBack() {
      if (eventStream !== source) return;
      closeEvents();
      showLive('offline', 'Reconnecting');
      refresh();
      retryEvents();
    }
    source.addEventListener('error', fallBack);
    source.addEventListener('snapshot', event => {
      if (eventStream !== source) return;
      let next;
      try {
        next = JSON.parse(event.data);
      } catch {
        fallBack();
        return;
      }
      // The Hub's snapshot endpoint can resolve a missing or stale registry ID.
      if (!next || typeof next !== 'object' || Array.isArray(next) ||
          (options.runs && (!Object.hasOwn(next, 'run_id') || (requestedRun && next.run_id !== requestedRun)))) {
        fallBack();
        return;
      }
      clearTimeout(timer);
      inFlight?.abort();
      applyLiveSnapshot(next, next.run_id);
      showLive('active', 'Live (SSE)');
      if (options.runs && activeRunId !== requestedRun) {
        closeEvents();
        if (!connectEvents()) refresh();
      }
    });
    return true;
  }
  function setPolling(active) {
    automatic = active;
    $('auto-refresh').setAttribute('aria-pressed', String(active));
    clearTimeout(timer);
    clearTimeout(qaTimer);
    closeEvents();
    if (!active) {
      inFlight?.abort();
      clearTimeout(runsTimer);
      runsRequest?.abort();
    }
    showLive(active ? 'active' : 'paused', active ? 'Live' : 'Paused');
    if (active) {
      if (!connectEvents() && options.live) timer = setTimeout(refresh, options.pollMs);
      if (options.runs) refreshRuns();
      if (options.qa) refreshQa();
    }
  }
  function setRunLocation(id) {
    const url = new URL(location.href);
    if (id) url.searchParams.set('run', id);
    else url.searchParams.delete('run');
    if (url.href !== location.href) history.replaceState(null, '', url);
  }
  function highlightRun() {
    for (const button of $('run-bar').querySelectorAll('.run-choice')) {
      button.setAttribute('aria-current', String(button.dataset.run === activeRunId));
    }
  }
  function switchRun(id) {
    activeRunId = id;
    setRunLocation(id);
    highlightRun();
    clearTimeout(timer);
    inFlight?.abort();
    closeEvents();
    clearLog();
    if (!automatic || !connectEvents()) refresh();
  }
  function runStatus(status) {
    if (status === 'missing' || blockedStatuses.has(status)) return 'FAILED';
    if (taskGroup(status) === 'done') return 'PASSED';
    return status === 'running' ? 'RUNNING' : 'WAITING';
  }
  const relativeTime = new Intl.RelativeTimeFormat(undefined, {numeric: 'auto', style: 'narrow'});
  function runTime(value) {
    const stamp = Date.parse(value);
    if (Number.isNaN(stamp)) return '';
    const elapsed = Math.max(0, (Date.now() - stamp) / 1000);
    if (elapsed < 10) return 'Just now';
    for (const [unit, size] of [['day', 86400], ['hour', 3600], ['minute', 60]]) {
      if (elapsed >= size) return relativeTime.format(-Math.floor(elapsed / size), unit);
    }
    return relativeTime.format(-Math.floor(elapsed), 'second');
  }
  function drawRuns(runs) {
    const bar = $('run-bar');
    const cards = new Map([...bar.querySelectorAll('.run-card')].map(card => [card.dataset.run, card]));
    const focused = document.activeElement;
    const next = runs.map(run => {
      let card = cards.get(run.id);
      if (!card) {
        card = element('div', '');
        card.className = 'run-card';
        card.dataset.run = run.id;
        const button = element('button', '');
        button.type = 'button';
        button.className = 'run-choice';
        button.dataset.run = run.id;
        const names = element('span', '');
        names.className = 'run-names';
        const project = element('span', '');
        project.className = 'run-project';
        names.append(element('strong', ''), project);
        const metadata = element('span', '');
        metadata.className = 'run-meta';
        metadata.append(element('span', ''), element('time', ''));
        button.append(names, metadata);
        card.append(button);
        if (options.readOnly) {
          button.style.borderRadius = '8px';
        } else {
          const dismiss = element('button', '×');
          dismiss.type = 'button';
          dismiss.className = 'run-dismiss';
          dismiss.title = 'Dismiss run';
          card.append(dismiss);
        }
      }
      const [button, dismiss] = card.children;
      const [names, metadata] = button.children;
      const [status, time] = metadata.children;
      status.className = 'run-status ' + statusClass(run.status === 'missing' ? 'failed' : run.status);
      status.textContent = runStatus(run.status);
      names.firstElementChild.textContent = run.name;
      const project = names.lastElementChild;
      project.replaceChildren();
      if (run.source_host) {
        const host = element('span', run.source_host);
        host.className = 'run-host';
        project.append(host, ' · ');
      }
      project.append(run.project_name);
      time.dateTime = run.updated_at || '';
      time.textContent = runTime(run.updated_at);
      button.setAttribute('aria-label', project.textContent + ' / ' + run.name + ', ' + statusLabel(run.status));
      dismiss?.setAttribute('aria-label', 'Dismiss ' + run.name);
      return card;
    });
    if (!next.length) next.push(element('p', 'No workflow runs yet'));
    if (next.length !== bar.children.length || next.some((item, index) => item !== bar.children[index])) {
      bar.replaceChildren(...next);
      if (bar.contains(focused)) focused.focus({preventScroll: true});
    }
    highlightRun();
  }
  let runsTimer, runsRequest;
  async function refreshRuns() {
    clearTimeout(runsTimer);
    runsRequest?.abort();
    const request = new AbortController();
    runsRequest = request;
    try {
      const response = await fetch(options.runs, {cache: 'no-store', signal: request.signal});
      if (!response.ok) throw new Error('HTTP ' + response.status);
      const {runs} = await response.json();
      if (!request.signal.aborted) drawRuns(runs);
    } catch {
      // A transient registry or network error leaves the current choices visible.
    } finally {
      if (runsRequest === request) {
        runsRequest = undefined;
        if (automatic && !request.signal.aborted) runsTimer = setTimeout(refreshRuns, Math.max(3000, options.pollMs));
      }
    }
  }
  async function dismissRun(button) {
    const card = button.closest('.run-card');
    const id = card.dataset.run;
    const feedback = $('run-feedback');
    button.disabled = true;
    feedback.hidden = true;
    try {
      const url = new URL(options.runs, location.href);
      url.pathname += '/' + encodeURIComponent(id);
      const response = await fetch(url, {method: 'DELETE'});
      if (!response.ok && response.status !== 404) throw new Error('HTTP ' + response.status);
      const fallback = (card.nextElementSibling || card.previousElementSibling)?.querySelector('.run-choice');
      if (document.activeElement === button || document.activeElement === document.body) {
        fallback?.focus({preventScroll: true});
      }
      card.remove();
      if (activeRunId === id) switchRun(null);
      await refreshRuns();
    } catch {
      button.disabled = false;
      feedback.textContent = 'Could not dismiss the run. Try again.';
      feedback.hidden = false;
    }
  }
  $('run-bar')?.addEventListener('click', event => {
    const button = event.target.closest('button');
    if (!button) return;
    if (button.classList.contains('run-dismiss')) {
      event.stopPropagation();
      dismissRun(button);
      return;
    }
    if (button.dataset.run === activeRunId && displayedRunId === activeRunId) return;
    switchRun(button.dataset.run);
  });

  function applyLiveSnapshot(next, runId) {
    const switched = options.runs && runId !== displayedRunId;
    if (options.runs) {
      displayedRunId = activeRunId = runId;
      setRunLocation(activeRunId);
      highlightRun();
    }
    if (switched) {
      select(null);
      drillStack = [];
      preferredInputPath = null;
      attentionSignature = undefined;
      fittedNodes = false;
      cameraMoved = false;
    }
    update(next, Boolean(switched));
    if (options.qa) refreshQa();
  }
  async function refresh() {
    if (!options.live) return;
    clearTimeout(timer);
    inFlight?.abort();
    const request = new AbortController();
    inFlight = request;
    try {
      const response = await fetch(liveUrl(options.live), {cache: 'no-store', signal: request.signal});
      if (!response.ok) throw new Error('HTTP ' + response.status);
      const next = await response.json();
      if (!request.signal.aborted) {
        applyLiveSnapshot(next, response.headers.get('X-KGraph-Run-ID') || next.run_id || null);
        if (!eventStream) {
          showLive(automatic ? 'active' : 'paused', automatic ? 'Live' : 'Paused');
        }
      }
    } catch {
      if (!request.signal.aborted && !eventStream) showLive('offline', 'Reconnecting');
    } finally {
      if (inFlight === request) {
        inFlight = undefined;
        if (automatic && !eventStream && !request.signal.aborted) timer = setTimeout(refresh, options.pollMs);
      }
    }
  }
  $('auto-refresh').addEventListener('click', () => setPolling(!automatic));
  update(bootstrap, true);
  if (options.runs) setRunLocation(activeRunId);
  if (options.live || options.events) {
    $('live-controls').hidden = false;
    setPolling(true);
  } else {
    if (options.qa) refreshQa();
    if (options.runs) refreshRuns();
  }
})();
