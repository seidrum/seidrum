// Seidrum Dashboard - Vanilla JS

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

function getApiKey() { return sessionStorage.getItem('seidrum_api_key') || ''; }
function setApiKey(key) { sessionStorage.setItem('seidrum_api_key', key); }

function handleLogin(e) {
    e.preventDefault();
    const key = document.getElementById('api-key-input').value.trim();
    if (key) {
        setApiKey(key);
        document.getElementById('login-prompt').style.display = 'none';
        showPage('overview');
    }
}

async function api(path, options = {}) {
    const resp = await fetch(path, {
        ...options,
        headers: {
            'Authorization': `Bearer ${getApiKey()}`,
            'Content-Type': 'application/json',
            ...options.headers,
        },
    });
    if (resp.status === 401) {
        document.getElementById('login-prompt').style.display = 'block';
        document.getElementById('page-content').innerHTML = '';
        throw new Error('Unauthorized');
    }
    return resp.json();
}

// ---------------------------------------------------------------------------
// Navigation
// ---------------------------------------------------------------------------

function showPage(page) {
    document.querySelectorAll('#sidebar a').forEach(a => a.classList.remove('active'));
    const link = document.querySelector(`#sidebar a[onclick="showPage('${page}')"]`);
    if (link) link.classList.add('active');

    const pages = { overview: renderOverview, plugins: renderPlugins, capabilities: renderCapabilities, storage: renderStorage, events: renderEvents };
    if (pages[page]) pages[page]();
}

function showToast(msg, type = 'success') {
    const el = document.createElement('div');
    el.className = `toast toast-${type}`;
    el.textContent = msg;
    document.body.appendChild(el);
    setTimeout(() => el.remove(), 3000);
}

// ---------------------------------------------------------------------------
// Overview
// ---------------------------------------------------------------------------

async function renderOverview() {
    const el = document.getElementById('page-content');
    el.innerHTML = '<h2>Loading...</h2>';

    try {
        const data = await api('/api/v1/dashboard/overview');
        const hours = Math.floor(data.uptime_seconds / 3600);
        const mins = Math.floor((data.uptime_seconds % 3600) / 60);

        const healthy = data.plugins.filter(p => p.health_status === 'healthy').length;
        const total = data.plugins.length;

        el.innerHTML = `
            <h2>System Overview</h2>
            <div class="grid">
                <div class="stat"><div class="value">${total}</div><div class="label">Plugins</div></div>
                <div class="stat"><div class="value">${healthy}</div><div class="label">Healthy</div></div>
                <div class="stat"><div class="value">${hours}h ${mins}m</div><div class="label">Uptime</div></div>
            </div>
            <div class="card">
                <h3>Registered Plugins</h3>
                <table>
                    <thead><tr><th>Name</th><th>Version</th><th>Status</th><th>Config</th></tr></thead>
                    <tbody>
                        ${data.plugins.map(p => `
                            <tr class="clickable" onclick="renderPluginDetail('${p.id}')">
                                <td><strong>${p.name}</strong><br><span style="color:var(--text-muted);font-size:12px">${p.id}</span></td>
                                <td>${p.version}</td>
                                <td><span class="badge badge-${p.health_status}">${p.health_status}</span></td>
                                <td>${p.has_config_schema ? '<span class="badge badge-tool">schema</span>' : ''}</td>
                            </tr>
                        `).join('')}
                    </tbody>
                </table>
            </div>
        `;
    } catch (e) {
        if (e.message !== 'Unauthorized') el.innerHTML = `<h2>Error</h2><p>${e.message}</p>`;
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

async function renderPlugins() {
    const el = document.getElementById('page-content');
    el.innerHTML = '<h2>Loading...</h2>';

    try {
        const data = await api('/api/v1/dashboard/overview');

        el.innerHTML = `
            <h2>Plugins (${data.plugins.length})</h2>
            <table>
                <thead><tr><th>ID</th><th>Name</th><th>Version</th><th>Status</th><th>Description</th></tr></thead>
                <tbody>
                    ${data.plugins.map(p => `
                        <tr class="clickable" onclick="renderPluginDetail('${p.id}')">
                            <td><code>${p.id}</code></td>
                            <td>${p.name}</td>
                            <td>${p.version}</td>
                            <td><span class="badge badge-${p.health_status}">${p.health_status}</span></td>
                            <td style="color:var(--text-muted)">${p.description}</td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        `;
    } catch (e) {
        if (e.message !== 'Unauthorized') el.innerHTML = `<h2>Error</h2><p>${e.message}</p>`;
    }
}

async function renderPluginDetail(pluginId) {
    const el = document.getElementById('page-content');
    el.innerHTML = '<h2>Loading...</h2>';

    try {
        const [configData, healthData] = await Promise.allSettled([
            api(`/api/v1/dashboard/plugins/${pluginId}/config`),
            api(`/api/v1/dashboard/plugins/${pluginId}/health`),
        ]);

        const config = configData.status === 'fulfilled' ? configData.value : {};
        const health = healthData.status === 'fulfilled' ? healthData.value : { status: 'unknown' };

        let html = `
            <h2>${pluginId}</h2>
            <p><a href="#" onclick="renderPlugins()">&larr; Back to plugins</a></p>

            <div class="card">
                <h3>Health</h3>
                <p>Status: <span class="badge badge-${health.status || 'unknown'}">${health.status || 'unknown'}</span></p>
                ${health.uptime_seconds ? `<p>Uptime: ${Math.floor(health.uptime_seconds / 60)} minutes</p>` : ''}
                ${health.events_processed ? `<p>Events processed: ${health.events_processed}</p>` : ''}
                ${health.last_error ? `<p style="color:var(--error)">Last error: ${health.last_error}</p>` : ''}
            </div>
        `;

        // Config form
        if (config.schema) {
            html += `<div class="card"><h3>Configuration</h3>`;
            html += renderConfigForm(config.schema, config.config || {}, pluginId);
            html += `</div>`;
        }

        el.innerHTML = html;
    } catch (e) {
        if (e.message !== 'Unauthorized') el.innerHTML = `<h2>Error</h2><p>${e.message}</p>`;
    }
}

// ---------------------------------------------------------------------------
// Config form generator
// ---------------------------------------------------------------------------

function renderConfigForm(schema, currentConfig, pluginId) {
    if (!schema.properties) return '<p style="color:var(--text-muted)">No configurable properties.</p>';

    const required = schema.required || [];
    let html = `<form id="config-form" onsubmit="submitConfig(event, '${pluginId}')">`;

    for (const [key, prop] of Object.entries(schema.properties)) {
        const value = currentConfig[key] !== undefined ? currentConfig[key] : (prop.default || '');
        const isRequired = required.includes(key);
        const label = key + (isRequired ? ' *' : '');

        html += `<div class="form-group">`;
        html += `<label for="cfg-${key}">${label}</label>`;

        if (prop.enum) {
            html += `<select id="cfg-${key}" name="${key}">`;
            for (const opt of prop.enum) {
                const selected = opt === value ? 'selected' : '';
                html += `<option value="${opt}" ${selected}>${opt}</option>`;
            }
            html += `</select>`;
        } else if (prop.type === 'boolean') {
            const checked = value ? 'checked' : '';
            html += `<input type="checkbox" id="cfg-${key}" name="${key}" ${checked} style="width:auto">`;
        } else if (prop.type === 'number' || prop.type === 'integer') {
            html += `<input type="number" id="cfg-${key}" name="${key}" value="${value}" step="${prop.type === 'integer' ? 1 : 'any'}">`;
        } else if (prop.type === 'object' || prop.type === 'array') {
            html += `<textarea id="cfg-${key}" name="${key}" rows="4">${typeof value === 'object' ? JSON.stringify(value, null, 2) : value}</textarea>`;
        } else {
            html += `<input type="text" id="cfg-${key}" name="${key}" value="${value}">`;
        }

        if (prop.description) html += `<div class="description">${prop.description}</div>`;
        html += `</div>`;
    }

    html += `<button type="submit">Save Configuration</button></form>`;
    return html;
}

async function submitConfig(e, pluginId) {
    e.preventDefault();
    const form = document.getElementById('config-form');
    const formData = new FormData(form);
    const config = {};

    // Get schema to know types
    const schemaResp = await api(`/api/v1/dashboard/plugins/${pluginId}/config/schema`);

    for (const [key, value] of formData.entries()) {
        const prop = schemaResp.properties?.[key] || {};
        if (prop.type === 'number') config[key] = parseFloat(value);
        else if (prop.type === 'integer') config[key] = parseInt(value, 10);
        else if (prop.type === 'boolean') config[key] = value === 'on';
        else if (prop.type === 'object' || prop.type === 'array') {
            try { config[key] = JSON.parse(value); } catch { config[key] = value; }
        } else config[key] = value;
    }

    // Handle unchecked checkboxes
    if (schemaResp.properties) {
        for (const [key, prop] of Object.entries(schemaResp.properties)) {
            if (prop.type === 'boolean' && !(key in config)) config[key] = false;
        }
    }

    try {
        const resp = await api(`/api/v1/dashboard/plugins/${pluginId}/config`, {
            method: 'PUT',
            body: JSON.stringify({ config }),
        });
        if (resp.success) showToast('Configuration saved');
        else showToast(resp.error || 'Save failed', 'error');
    } catch (e) {
        showToast(e.message, 'error');
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

async function renderCapabilities() {
    const el = document.getElementById('page-content');
    el.innerHTML = '<h2>Loading...</h2>';

    try {
        const data = await api('/api/v1/capabilities?limit=100');

        el.innerHTML = `
            <h2>Capabilities (${data.tools.length})</h2>
            <table>
                <thead><tr><th>ID</th><th>Name</th><th>Kind</th><th>Summary</th></tr></thead>
                <tbody>
                    ${data.tools.map(t => `
                        <tr>
                            <td><code>${t.tool_id}</code></td>
                            <td>${t.name}</td>
                            <td><span class="badge badge-${t.kind}">${t.kind}</span></td>
                            <td style="color:var(--text-muted)">${t.summary_md}</td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        `;
    } catch (e) {
        if (e.message !== 'Unauthorized') el.innerHTML = `<h2>Error</h2><p>${e.message}</p>`;
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

async function renderStorage() {
    const el = document.getElementById('page-content');
    el.innerHTML = `
        <h2>Plugin Storage</h2>
        <div class="card">
            <div style="display:flex;gap:8px;margin-bottom:16px">
                <input type="text" id="storage-plugin" placeholder="Plugin ID" style="flex:1">
                <input type="text" id="storage-namespace" placeholder="Namespace" value="default" style="flex:1">
                <button onclick="listStorageKeys()">List Keys</button>
            </div>
            <div id="storage-results"></div>
        </div>
    `;
}

async function listStorageKeys() {
    const pluginId = document.getElementById('storage-plugin').value.trim();
    const namespace = document.getElementById('storage-namespace').value.trim() || 'default';
    const results = document.getElementById('storage-results');

    if (!pluginId) { results.innerHTML = '<p style="color:var(--text-muted)">Enter a plugin ID</p>'; return; }

    try {
        const data = await api('/api/v1/storage/list', {
            method: 'POST',
            body: JSON.stringify({ plugin_id: pluginId, namespace }),
        });

        if (data.keys.length === 0) {
            results.innerHTML = '<p style="color:var(--text-muted)">No keys found.</p>';
            return;
        }

        results.innerHTML = `
            <table>
                <thead><tr><th>Key</th><th>Actions</th></tr></thead>
                <tbody>
                    ${data.keys.map(k => `
                        <tr>
                            <td><code>${k}</code></td>
                            <td><button class="btn-sm" onclick="viewStorageKey('${pluginId}', '${namespace}', '${k}')">View</button></td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        `;
    } catch (e) {
        results.innerHTML = `<p style="color:var(--error)">${e.message}</p>`;
    }
}

async function viewStorageKey(pluginId, namespace, key) {
    try {
        const data = await api('/api/v1/storage/get', {
            method: 'POST',
            body: JSON.stringify({ plugin_id: pluginId, namespace, key }),
        });
        const value = data.found ? JSON.stringify(data.value, null, 2) : '(not found)';
        alert(`${pluginId}/${namespace}/${key}:\n\n${value}`);
    } catch (e) {
        alert('Error: ' + e.message);
    }
}

// ---------------------------------------------------------------------------
// Events (live stream)
// ---------------------------------------------------------------------------

let eventWs = null;

function renderEvents() {
    const el = document.getElementById('page-content');
    el.innerHTML = `
        <h2>Live Events</h2>
        <div class="card">
            <div style="display:flex;gap:8px;margin-bottom:16px">
                <input type="text" id="event-subject" placeholder="Subject pattern (e.g., channel.*.inbound)" value="channel.*.inbound" style="flex:1">
                <button onclick="startEventStream()">Subscribe</button>
                <button class="btn-danger" onclick="stopEventStream()">Stop</button>
            </div>
            <div id="event-log" class="event-log"></div>
        </div>
    `;
}

function startEventStream() {
    stopEventStream();
    const subject = document.getElementById('event-subject').value.trim();
    if (!subject) return;

    const wsUrl = `ws://${location.host}/ws?api_key=${encodeURIComponent(getApiKey())}`;
    eventWs = new WebSocket(wsUrl);

    eventWs.onopen = () => {
        eventWs.send(JSON.stringify({
            type: 'register',
            plugin: { id: 'dashboard-monitor', name: 'Dashboard Monitor', version: '0.1.0', description: 'Live event viewer' }
        }));
        setTimeout(() => {
            eventWs.send(JSON.stringify({ type: 'subscribe', subjects: [subject] }));
        }, 500);
    };

    eventWs.onmessage = (msg) => {
        const data = JSON.parse(msg.data);
        if (data.type === 'event') {
            const log = document.getElementById('event-log');
            const time = new Date().toLocaleTimeString();
            const entry = document.createElement('div');
            entry.className = 'event-entry';
            entry.innerHTML = `<span class="event-time">${time}</span><span class="event-subject">${data.subject}</span>
                <pre style="margin-top:4px;font-size:11px;color:var(--text-muted);white-space:pre-wrap">${JSON.stringify(data.payload, null, 2).slice(0, 500)}</pre>`;
            log.prepend(entry);
            // Keep max 100 entries
            while (log.children.length > 100) log.lastChild.remove();
        }
    };

    eventWs.onerror = () => showToast('WebSocket error', 'error');
}

function stopEventStream() {
    if (eventWs) {
        try { eventWs.send(JSON.stringify({ type: 'deregister' })); } catch {}
        eventWs.close();
        eventWs = null;
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

(function init() {
    if (!getApiKey()) {
        document.getElementById('login-prompt').style.display = 'block';
    } else {
        showPage('overview');
    }
})();
