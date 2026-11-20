// Adds an authorized domain to the Firebase project's Auth config, using the existing
// Firebase CLI login (~/.config/configstore/firebase-tools.json). Tokens stay in memory.
//   node deploy/firebase-domain.mjs <project-id> <domain>
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const [project, domain] = process.argv.slice(2);
if (!project || !domain) throw new Error('usage: firebase-domain.mjs <project-id> <domain>');

// The Firebase CLI's public OAuth client (firebase-tools src/api.ts).
const CLIENT_ID = '<firebase-tools CLIENT_ID>';
const CLIENT_SECRET = '<firebase-tools CLIENT_SECRET>';

const store = JSON.parse(readFileSync(join(homedir(), '.config/configstore/firebase-tools.json'), 'utf8'));
const refresh = store.tokens?.refresh_token;
if (!refresh) throw new Error('run `firebase login` first');

const tokenResponse = await fetch('https://oauth2.googleapis.com/token', {
  method: 'POST',
  headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
  body: new URLSearchParams({
    grant_type: 'refresh_token',
    refresh_token: refresh,
    client_id: CLIENT_ID,
    client_secret: CLIENT_SECRET,
  }),
});
if (!tokenResponse.ok) throw new Error(`token refresh failed: HTTP ${tokenResponse.status}`);
const { access_token: token } = await tokenResponse.json();

async function call(url, method = 'GET', body) {
  const response = await fetch(url, {
    method,
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json', 'X-Goog-User-Project': project },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) throw new Error(`${method} ${url}: HTTP ${response.status} ${await response.text()}`);
  return response.json();
}

const base = `https://identitytoolkit.googleapis.com/admin/v2/projects/${project}/config`;
const config = await call(base);
const domains = config.authorizedDomains ?? [];
if (domains.includes(domain)) {
  console.log(`${domain} is already authorized`);
} else {
  const updated = await call(`${base}?updateMask=authorizedDomains`, 'PATCH', {
    authorizedDomains: [...domains, domain],
  });
  console.log(`authorized domains: ${updated.authorizedDomains.join(', ')}`);
}
