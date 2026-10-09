const bytesToBase64 = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes)));
export class AllumetteError extends Error {
  constructor(status, code) { super(code); this.status = status; this.code = code; }
}
/** Engine-independent, in-memory guest identity and explicit browser-session API. */
export class AllumetteBrowserClient {
  constructor(baseUrl, { fetch: fetchImpl = globalThis.fetch } = {}) {
    const url = new URL(baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== '/') throw new Error('Invalid API origin');
    this.baseUrl = url.origin; this.fetch = fetchImpl; this.token = null;
  }
  async request(path, method = 'GET', body, signal) {
    const response = await this.fetch(`${this.baseUrl}${path}`, { method, cache: 'no-store', signal, headers: { ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}), ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) }, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    const text = await response.text();
    let json; try { json = JSON.parse(text); } catch { json = null; }
    if (!response.ok) { if (response.status === 401) this.token = null; throw new AllumetteError(response.status, json?.code || text || 'request_failed'); }
    return json;
  }
  async loginGuest(username) {
    if (!username.trim() || username.trim().length > 32) throw new Error('invalid_username');
    const key = await crypto.subtle.generateKey({name:'Ed25519'}, true, ['sign','verify']);
    const {challenge} = await this.request('/auth/challenge', 'POST');
    const signature = await crypto.subtle.sign('Ed25519', key.privateKey, new TextEncoder().encode(challenge));
    const publicKey = bytesToBase64(await crypto.subtle.exportKey('raw',key.publicKey));
    const {token} = await this.request('/auth/login','POST',{username:username.trim(),challenge,public_key_b64:publicKey,signature_b64:bytesToBase64(signature)});
    this.token = token;
    // No durable private key or JWT storage; each client instance is a guest session.
    return {player_id:publicKey,username:username.trim()};
  }
  listLobbies(signal) { return this.request('/browser/v1/lobbies','GET',undefined,signal); }
  createLobby(gameId, compatibilityId, capacity = 2) { return this.request('/browser/v1/lobbies','POST',{game_id:gameId,compatibility_id:compatibilityId,capacity}); }
  createInvitation(id) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/invitations`,'POST'); }
  joinLobby(id, invitationToken, compatibilityId) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/join`,'POST',{invitation_token:invitationToken,compatibility_id:compatibilityId}); }
  setReady(id, ready, compatibilityId) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/ready`,'POST',{ready,compatibility_id:compatibilityId}); }
  startSession(id) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/start`,'POST'); }
  async getSessionContext(id, signal) {
    const [context, iceServers] = await Promise.all([this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/session`,'GET',undefined,signal),this.request('/ice-servers','GET',undefined,signal)]);
    if (context.schema_version !== 1) throw new Error('unsupported_session');
    const origin = new URL(this.baseUrl); origin.protocol = origin.protocol === 'https:' ? 'wss:' : 'ws:';
    return {...context, signaling_url: `${origin.origin}/${context.signaling_path}`, ice_servers:iceServers};
  }
  endSession(id) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}/session`,'DELETE'); }
  leaveLobby(id) { return this.request(`/browser/v1/lobbies/${encodeURIComponent(id)}`,'DELETE'); }
  logout() { this.token = null; }
}
