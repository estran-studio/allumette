import { writable, get, derived } from 'svelte/store';
import * as bip39 from 'bip39';
import * as ed from '@noble/ed25519';
import { sha512 } from '@noble/hashes/sha2.js';
import { ethers } from 'ethers';
import { toast } from '@zerodevx/svelte-toast';
import { deriveEncryptionKey, encryptData, decryptData } from './crypto-utils.js';

// Enable synchronous methods for ed25519
ed.hashes.sha512 = sha512;
ed.hashes.sha512Async = (m) => Promise.resolve(sha512(m));

// --- Configuration ---
const API_BASE_URL = writable('http://localhost:3536'); // Default, user can change this
let apiBaseUrlValue;
API_BASE_URL.subscribe(value => apiBaseUrlValue = value);

const DEFAULT_ICE_SERVERS = [{ urls: ["stun:stun.l.google.com:19302"] }];


// --- State Management ---
// Check if we're in a browser environment (compatible with all bundlers)
const browser = typeof window !== 'undefined';

export const isLoggedIn = writable(false);
export const currentUser = writable(null);
export const jwt = writable(browser ? localStorage.getItem('allumette-jwt') : null);
export const recoveryPhrase = writable(browser ? localStorage.getItem('allumette-recovery') : null);
// Encrypted data management
export const encryptionKey = writable(null);
// Internal writable store for friends
const _friendsStore = writable([]);

// Expose as readable store (matches friendsStore specification)
export const friendsStore = {
    subscribe: _friendsStore.subscribe
};

// Keep friendsList as alias for backward compatibility
export const friendsList = _friendsStore;
export const lobbies = writable([]);

// SSE connection for lobby updates
let lobbyEventSource = null;
let lobbyStreamController = null; // AbortController for fetch-based SSE
let lobbyReconnectTimeout = null;


// --- Subscriptions ---

// Automatically update login status when JWT changes
jwt.subscribe(token => {
    if (!browser) return; // Don't run on the server

    // No token -> clear state
    if (!token) {
        isLoggedIn.set(false);
        currentUser.set(null);
        localStorage.removeItem('allumette-jwt');
        return;
    }

    // Try to decode the token and validate expiration
    const claims = decodeJWT(token);
    if (!claims) {
        // Invalid token: clear and notify
        jwt.set(null);
        isLoggedIn.set(false);
        currentUser.set(null);
        localStorage.removeItem('allumette-jwt');
        try { toast.push('Invalid session. Please log in again.'); } catch (e) { /* ignore */ }
        return;
    }

    // If token has an expiration, check it (exp is seconds since epoch)
    const now = Math.floor(Date.now() / 1000);
    if (claims.exp && typeof claims.exp === 'number' && now >= claims.exp) {
        // Token expired
        jwt.set(null);
        isLoggedIn.set(false);
        currentUser.set(null);
        localStorage.removeItem('allumette-jwt');
        try { toast.push('Your session has expired. Please log in again.'); } catch (e) { /* ignore */ }
        return;
    }

    // Token is valid
    isLoggedIn.set(true);
    localStorage.setItem('allumette-jwt', token);
    currentUser.set({
        username: claims.username,
        publicKey: claims.sub,
        isWallet: false,
    });
});

// Store recovery phrase securely
recoveryPhrase.subscribe(async (phrase) => {
  if (!browser) return;
  if (phrase) {
    localStorage.setItem('allumette-recovery', phrase);
    // If we have a recovery phrase, we can try to derive the encryption key automatically
    try {
        const seed = await bip39.mnemonicToSeed(phrase);
        const secretKeyBytes = seed.slice(0, 32);
        // We use the raw bytes of the secret key (derived from seed) to derive the encryption key
        // Note: This matches the key derivation in createAccount/recoverAccount
        const key = await deriveEncryptionKey(secretKeyBytes);
        encryptionKey.set(key);
    } catch (e) {
        console.error("Failed to auto-derive encryption key from recovery phrase", e);
    }
  } else {
    localStorage.removeItem('allumette-recovery');
  }
});

// Persist friends list to localStorage (Encrypted)
let friendsListUnsubscribe;
let initialLoadComplete = false; // Track if we've loaded data to prevent race conditions

const dataManager = derived([encryptionKey, currentUser], ([$encryptionKey, $currentUser]) => {
    return { key: $encryptionKey, user: $currentUser };
});

dataManager.subscribe(async ({ key, user }) => {
    if (!browser) return;

    // Cleanup previous subscription
    if (friendsListUnsubscribe) {
        friendsListUnsubscribe();
        friendsListUnsubscribe = null;
    }

    if (key && user?.publicKey) {
        const storageKey = `allumette-data-${user.publicKey}`;

        // 1. Try to load existing data
        const storedData = localStorage.getItem(storageKey);
        if (storedData) {
            try {
                const encryptedObj = JSON.parse(storedData);
                const data = await decryptData(encryptedObj, key);
                if (data.friends) {
                    // Migrate old format (username) to new format (name)
                    const migratedFriends = data.friends.map(f => ({
                        publicKey: f.publicKey,
                        name: f.name || f.username, // Support both old and new format
                        addedAt: f.addedAt
                    }));
                    _friendsStore.set(migratedFriends);
                }
            } catch (e) {
                console.error("Failed to decrypt user data:", e);
                toast.push("Failed to load encrypted data. Bad key?", { classes: ['error-toast'] });
            }
        } else {
             // Initialize empty if nothing stored
             _friendsStore.set([]);
        }

        // Mark initial load as complete - now safe to save
        initialLoadComplete = true;

        // 2. Subscribe to changes and save encrypted
        // Skip the first immediate invocation (which is the data we just loaded)
        let isFirstRun = true;
        friendsListUnsubscribe = _friendsStore.subscribe(async (list) => {
            // Skip the initial subscription callback (it fires immediately with current value)
            if (isFirstRun) {
                isFirstRun = false;
                return;
            }

            // Only save if initial load is complete (prevents race condition overwrites)
            if (!initialLoadComplete) {
                return;
            }

            try {
                const dataToSave = { friends: list };
                const encrypted = await encryptData(dataToSave, key);
                localStorage.setItem(storageKey, JSON.stringify(encrypted));
            } catch (e) {
                console.error("Failed to save encrypted data:", e);
            }
        });
    } else {
        // No key or no user: clear list in memory to prevent leakage
        // Do NOT save - the encrypted data in localStorage should remain untouched
        // Reset the load flag so we don't accidentally save during the race window
        initialLoadComplete = false;
        _friendsStore.set([]);
    }
});


// --- Helper Functions ---

// Decode JWT (without verification - only for extracting claims)
function decodeJWT(token) {
  try {
    const parts = token.split('.');
    if (parts.length !== 3) return null;
    let payload = parts[1];
    // base64url -> base64
    payload = payload.replace(/-/g, '+').replace(/_/g, '/');
    // pad
    const pad = payload.length % 4;
    if (pad) payload += '='.repeat(4 - pad);

    let decoded;
    // Prefer Buffer when available (Node), otherwise use browser APIs
    if (typeof Buffer !== 'undefined' && typeof Buffer.from === 'function') {
      decoded = Buffer.from(payload, 'base64').toString('utf8');
    } else if (typeof atob === 'function') {
      const binary = atob(payload);
      const len = binary.length;
      const bytes = new Uint8Array(len);
      for (let i = 0; i < len; i++) bytes[i] = binary.charCodeAt(i);
      decoded = new TextDecoder().decode(bytes);
    } else {
      // Fallback: try using globalThis
      const g = typeof globalThis !== 'undefined' ? globalThis : window || {};
      if (g.Buffer && typeof g.Buffer.from === 'function') {
        decoded = g.Buffer.from(payload, 'base64').toString('utf8');
      } else {
        throw new Error('No base64 decoder available');
      }
    }

    return JSON.parse(decoded);
  } catch (e) {
    console.error('Failed to decode JWT:', e);
    return null;
  }
}

// Helper function for bytes to hex
function bytesToHex(bytes) {
  return Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
}

// Helper function for hex to bytes
function hexToBytes(hex) {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substr(i, 2), 16);
  }
  return bytes;
}

// Helper function for base64 encoding
function base64Encode(bytes) {
  return btoa(String.fromCharCode(...bytes));
}

// Helper function for base64 decoding
function base64Decode(str) {
    const binaryString = atob(str);
    const bytes = new Uint8Array(binaryString.length);
    for (let i = 0; i < binaryString.length; i++) {
        bytes[i] = binaryString.charCodeAt(i);
    }
    return bytes;
}

/**
 * Derives a salt from a username using SHA-256.
 * @param {string} username - The username.
 * @returns {Promise<Uint8Array>} The salt (first 16 bytes of SHA-256 hash).
 */
const getSalt = async (username) => {
    const encoder = new TextEncoder();
    const data = encoder.encode(username);
    const hashBuffer = await crypto.subtle.digest('SHA-256', data);
    const hashArray = new Uint8Array(hashBuffer);
    return hashArray.slice(0, 16); // Use first 16 bytes as salt
};

/**
 * Derives a private key from a secret using Argon2.
 * @param {string} username - The username (used for salt derivation).
 * @param {string} secret - The user's secret/password.
 * @returns {Promise<Uint8Array>} The 32-byte private key.
 */
export async function getPrivateKey(username, secret) {
    if (!argon2) {
        throw new Error('Argon2 not initialized. Please wait for the module to load.');
    }
    
    const salt = await getSalt(username);
    const hash = await argon2.hash({
        pass: secret,
        salt: salt,
        time: 2,
        mem: 64 * 1024,
        hashLen: 32,
        parallelism: 1,
        type: argon2.ArgonType?.Argon2id || 2, // Argon2id = 2
    });
    return hash.hash;
}

/**
 * Generates a random secret key (32 bytes as hex).
 * @returns {string} A random secret key in hex format.
 */
function generateSecretKey() {
    const bytes = new Uint8Array(32);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
}

/**
 * Generates a recovery key (same as secret key, 64-char hex string).
 * @returns {string} A random recovery key in hex format.
 */
function generateRecoveryKey() {
    const bytes = new Uint8Array(32);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
}


// --- Service Methods ---

/**
 * Creates a new account by generating a random secret key and recovery key.
 * @param {string} username - The desired username.
 * @returns {Promise<{token: string, recoveryPhrase: string, secretKey: string}>}
 */
export async function createAccount(username) {
    // 1. Generate a mnemonic phrase which will be the master recovery key.
    const mnemonic = bip39.generateMnemonic();

    // 2. Derive a deterministic seed from the mnemonic.
    const seed = await bip39.mnemonicToSeed(mnemonic);
    const secretKeyBytes = seed.slice(0, 32);

    // 3. Use the first 32 bytes of the seed as the secret key.
    const secretKey = bytesToHex(secretKeyBytes);

    // 4. Login with the derived secret to register the public key on the server.
    const token = await loginWithSecret(username, secretKey);

    // 5. Store the mnemonic phrase for the session.
    recoveryPhrase.set(mnemonic);
    
    // 6. Set encryption key (derived from the same secret source)
    // We use the raw bytes to ensure consistency.
    const encKey = await deriveEncryptionKey(secretKeyBytes);
    encryptionKey.set(encKey);

    return {
        token,
        recoveryPhrase: mnemonic, // Return the mnemonic to the user.
        secretKey, // Also return the derived secret key for immediate use.
    };
}

/**
 * Logs in a user with their username and secret.
 * @param {string} username - The user's username.
 * @param {string} secret - The user's secret key (hex string).
 */
export async function loginWithSecret(username, secret) {
    // 1. Derive private key from username + secret
    const privateKey = await getPrivateKey(username, secret);
    const publicKey = await ed.getPublicKeyAsync(privateKey);
    const publicKeyB64 = base64Encode(publicKey);

    // 2. Get challenge
    const challengeResponse = await fetch(`${apiBaseUrlValue}/auth/challenge`, {
        method: 'POST',
    });
    if (!challengeResponse.ok) {
        const error = await challengeResponse.text();
        throw new Error(`Failed to get challenge: ${error}`);
    }
    const { challenge } = await challengeResponse.json();

    // 3. Sign challenge (convert string to bytes first)
    const encoder = new TextEncoder();
    const challengeBytes = encoder.encode(challenge);
    const signature = await ed.signAsync(challengeBytes, privateKey);
    const signatureB64 = base64Encode(signature);

    // 4. Request JWT
    const loginResponse = await fetch(`${apiBaseUrlValue}/auth/login`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            public_key_b64: publicKeyB64,
            username: username,
            challenge: challenge,
            signature_b64: signatureB64,
        }),
    });

    if (!loginResponse.ok) {
        const error = await loginResponse.text();
        throw new Error(`Login failed: ${error}`);
    }

    const { token } = await loginResponse.json();
    jwt.set(token);
    // currentUser will be set automatically by JWT subscription
    
    // 5. Derive and set encryption key
    // Important: We need a stable key for encryption.
    // If the user logs in with SECRET, we use the secret bytes as the seed.
    // Note: This must match how createAccount/recoverAccount derive it.
    // In those functions, they derived it from the SEED (from mnemonic).
    // Here we only have the SECRET (hex string). 
    // Is secretKey == bytesToHex(seed.slice(0,32))? Yes.
    // So we can convert hex back to bytes.
    try {
        const secretBytes = hexToBytes(secret);
        const encKey = await deriveEncryptionKey(secretBytes);
        encryptionKey.set(encKey);
    } catch (e) {
        console.warn("Could not derive encryption key from secret (maybe wallet login?)", e);
    }

    return token;
}

/**
 * Initiates login process using a browser wallet (e.g., MetaMask).
 * WARNING: This feature requires backend implementation of /api/challenge/eth/:address
 * and /api/login/eth endpoints, which are not yet implemented.
 */
export async function loginWithWallet() {
    if (!window.ethereum) {
        throw new Error('No crypto wallet found. Please install a wallet extension like MetaMask.');
    }

    try {
        const provider = new ethers.BrowserProvider(window.ethereum);
        const signer = await provider.getSigner();
        const address = await signer.getAddress();

        // 1. Get challenge (assuming a different endpoint for ETH addresses)
        const challengeResponse = await fetch(`${apiBaseUrlValue}/api/challenge/eth/${address}`);
        if (!challengeResponse.ok) {
            throw new Error('Failed to get challenge for wallet address. The server may not support wallet login yet.');
        }
        const { challenge } = await challengeResponse.json();

        // 2. Sign challenge
        const signature = await signer.signMessage(challenge);

        // 3. Request JWT (assuming a different endpoint for ETH addresses)
        const loginResponse = await fetch(`${apiBaseUrlValue}/api/login/eth`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                address: address,
                signature: signature,
            }),
        });

        if (!loginResponse.ok) {
            const error = await loginResponse.text();
            throw new Error(`Wallet login failed: ${error}`);
        }

        const { token } = await loginResponse.json();
        jwt.set(token);
        // For wallet users, the username can be their address
        currentUser.set({ username: address, publicKey: address, isWallet: true });
        
        // For Wallet Login, we don't have a stable "secret" to derive the encryption key.
        // We could sign a constant message "allumette-encryption-key" to get a stable signature?
        // For now, wallet users might not get encrypted persistence across sessions unless we implement signature-based derivation.
        // Let's implement signature-based derivation:
        try {
            const encKeySig = await signer.signMessage("Allumette Local Storage Encryption Key");
            // Use the signature hash as the key source
            const encKeyBytes = ethers.getBytes(ethers.keccak256(ethers.toUtf8Bytes(encKeySig)));
            const encKey = await deriveEncryptionKey(encKeyBytes);
            encryptionKey.set(encKey);
        } catch (e) {
            console.warn("Failed to derive encryption key for wallet:", e);
        }

        return token;
    } catch (err) {
        console.error("Wallet login error:", err);
        throw err;
    }
}

/**
 * Recovers an account using the recovery phrase.
 * Derives the secret key from the mnemonic and logs in.
 * @param {string} username - The username.
 * @param {string} mnemonic - The 24-word recovery phrase.
 */
export async function recoverAccount(username, mnemonic) {
    if (!bip39.validateMnemonic(mnemonic)) {
        throw new Error('Invalid recovery phrase');
    }
    
    // Derive seed from mnemonic, same as in createAccount
    const seed = await bip39.mnemonicToSeed(mnemonic);
    const secretKeyBytes = seed.slice(0, 32);
    
    // Use first 32 bytes as the secret key
    const secretKey = bytesToHex(secretKeyBytes);
    
    // Login with the recovered secret
    const token = await loginWithSecret(username, secretKey);
    
    // Store the recovery phrase for the new session
    recoveryPhrase.set(mnemonic);
    
    return token;
}

/**
 * Logs the current user out.
 */
export function logout() {
    disconnectLobbyStream();
    jwt.set(null);
    recoveryPhrase.set(null);
    encryptionKey.set(null);
    _friendsStore.set([]);
}

// --- Friend Management ---

/**
 * Adds a friend directly by public key and name.
 * @param {string} publicKey - The friend's public key.
 * @param {string} name - Display name for this friend.
 */
export function addFriend(publicKey, name) {
    const currentFriends = get(_friendsStore);
    if (currentFriends.some(friend => friend.publicKey === publicKey)) {
        throw new Error('Friend already exists.');
    }
    _friendsStore.update(list => [...list, {
        publicKey,
        name,
        addedAt: Date.now()
    }]);
}

/**
 * Removes a friend by their public key.
 * @param {string} publicKey - The public key of the friend to remove.
 */
export function removeFriend(publicKey) {
    _friendsStore.update(list => list.filter(friend => friend.publicKey !== publicKey));
}

/**
 * Updates a friend's display name.
 * @param {string} publicKey - The public key of the friend to update.
 * @param {string} name - The new display name.
 */
export function updateFriendName(publicKey, name) {
    _friendsStore.update(list =>
        list.map(f => f.publicKey === publicKey ? { ...f, name } : f)
    );
}

/**
 * Generates a friend code for the current user.
 * A friend code is a base64 encoded JSON string containing the user's name and public key.
 * @returns {string} The friend code.
 */
export function generateMyFriendCode() {
    const user = get(currentUser);
    if (!user) {
        throw new Error('User not logged in.');
    }
    const friendInfo = {
        name: user.username, // Use username as the default name in friend code
        publicKey: user.publicKey
    };
    const json = JSON.stringify(friendInfo);
    const bytes = new TextEncoder().encode(json);
    return base64Encode(bytes);
}

/**
 * Adds a friend from a friend code.
 * @param {string} friendCode - The friend code to add.
 * @throws {Error} If the friend code is invalid or the friend already exists.
 */
export function addFriendFromCode(friendCode) {
    try {
        const bytes = base64Decode(friendCode);
        const json = new TextDecoder().decode(bytes);
        const friendInfo = JSON.parse(json);

        // Support both old format (username) and new format (name)
        const name = friendInfo.name || friendInfo.username;
        if (!name || !friendInfo.publicKey) {
            throw new Error('Invalid friend code format.');
        }

        const currentFriends = get(_friendsStore);
        if (currentFriends.some(friend => friend.publicKey === friendInfo.publicKey)) {
            throw new Error('Friend already exists.');
        }

        _friendsStore.update(list => [...list, {
            publicKey: friendInfo.publicKey,
            name,
            addedAt: Date.now()
        }]);

    } catch (e) {
        console.error('Failed to add friend:', e);
        throw new Error('Invalid or malformed friend code.');
    }
}

// --- Lobby Management ---

/**
 * Fetches ICE servers (TURN/STUN) configuration.
 * @returns {Promise<object[]>} Array of RTCIceServer objects.
 */
export async function getIceServers() {
    const token = get(jwt);

    if (!token) {
        console.warn('No JWT token for ICE server fetch, defaulting to STUN only.');
        return DEFAULT_ICE_SERVERS;
    }

    try {
        const response = await fetch(`${apiBaseUrlValue}/ice-servers`, {
            headers: { 'Authorization': `Bearer ${token}` },
        });
        if (!response.ok) {
            throw new Error(`Failed to fetch ICE servers: ${response.status} ${response.statusText}`);
        }
        return await response.json();
    } catch (e) {
        console.warn('Could not fetch ICE servers, defaulting to STUN only:', e);
        return DEFAULT_ICE_SERVERS;
    }
}

/**
 * Fetches the list of all lobbies (one-time fetch).
 */
export async function getLobbies() {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const response = await fetch(`${apiBaseUrlValue}/lobbies`, {
        headers: { 'Authorization': `Bearer ${token}` },
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to get lobbies: ${error}`);
    }

    const lobbyData = await response.json();
    lobbies.set(lobbyData);
    return lobbyData;
}

/**
 * Connects to the SSE stream for real-time lobby updates.
 */
export function connectLobbyStream() {
    const token = get(jwt);
    if (!token) {
        console.warn('Cannot connect to lobby stream: not logged in');
        return;
    }
    // ensure previous stream is torn down
    disconnectLobbyStream();
    // Use fetch API (with Authorization header) to establish SSE-like streaming
    connectLobbyStreamWithFetch(token);
}

/**
 * Internal function to connect to SSE stream using fetch API.
 */
async function connectLobbyStreamWithFetch(token) {
    // cancel any pending reconnect timer
    if (lobbyReconnectTimeout) {
        clearTimeout(lobbyReconnectTimeout);
        lobbyReconnectTimeout = null;
    }

    try {
        const controller = new AbortController();
        lobbyStreamController = controller;

        const response = await fetch(`${apiBaseUrlValue}/lobbies/stream`, {
            headers: {
                'Authorization': `Bearer ${token}`,
                'Accept': 'text/event-stream',
            },
            signal: controller.signal,
        });

        if (!response.ok) {
            throw new Error(`Failed to connect to lobby stream: ${response.statusText}`);
        }

        if (!response.body) {
            throw new Error('Readable stream not available');
        }

        // SSE only broadcasts changes, so restore a snapshot on every connection.
        await getLobbies();
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';

        while (true) {
            const { done, value } = await reader.read();
            if (done) {
                // A normal EOF also means the stream was lost. Resynchronize on reconnect.
                if (!controller.signal.aborted) throw new Error('Lobby stream ended');
                break;
            }
            buffer += decoder.decode(value, { stream: true });

            // Parse complete SSE events separated by a blank line (\n\n or \r\n\r\n)
            let sepPos = -1;
            // find earliest separator
            const idx1 = buffer.indexOf('\r\n\r\n');
            const idx2 = buffer.indexOf('\n\n');
            if (idx1 !== -1 && (idx2 === -1 || idx1 < idx2)) sepPos = idx1;
            else if (idx2 !== -1) sepPos = idx2;

            while (sepPos !== -1) {
                let sepLen = buffer.startsWith('\r\n\r\n', sepPos) ? 4 : 2;
                const eventText = buffer.slice(0, sepPos);
                buffer = buffer.slice(sepPos + sepLen);

                // collect data: lines
                const lines = eventText.split(/\r?\n/);
                const dataParts = [];
                for (const line of lines) {
                    const t = line.trimRight();
                    if (t.startsWith('data:')) {
                        dataParts.push(t.slice(5).trimStart());
                    }
                }
                if (dataParts.length > 0) {
                    const dataStr = dataParts.join('\n');
                    try {
                        const parsed = JSON.parse(dataStr);
                        lobbies.set(parsed);
                    } catch (e) {
                        console.error('Failed to parse SSE data for lobbies:', e, dataStr);
                    }
                }

                const idx1b = buffer.indexOf('\r\n\r\n');
                const idx2b = buffer.indexOf('\n\n');
                if (idx1b !== -1 && (idx2b === -1 || idx1b < idx2b)) sepPos = idx1b;
                else if (idx2b !== -1) sepPos = idx2b;
                else sepPos = -1;
            }
        }
    } catch (err) {
        if (err && err.name === 'AbortError') {
            // normal disconnect
            return;
        }
        console.error('Lobby stream error:', err);
        // schedule reconnect
        lobbyStreamController = null;
        lobbyReconnectTimeout = setTimeout(() => {
            const currentToken = get(jwt);
            if (currentToken) connectLobbyStreamWithFetch(currentToken);
        }, 3000);
    }
}

/**
 * Disconnects from the SSE lobby stream.
 */
export function disconnectLobbyStream() {
    // Abort fetch-based stream if present
    if (lobbyStreamController) {
        try { lobbyStreamController.abort(); } catch (e) { /* ignore */ }
        lobbyStreamController = null;
    }
    if (lobbyReconnectTimeout) {
        clearTimeout(lobbyReconnectTimeout);
        lobbyReconnectTimeout = null;
    }
    if (lobbyEventSource) {
        try { lobbyEventSource.close(); } catch (e) { /* ignore */ }
        lobbyEventSource = null;
    }
}

// Auto-manage stream when jwt changes (connect on login, disconnect on logout)
jwt.subscribe(token => {
    if (!browser) return;
    if (token) {
        // connect after small delay to let login finish
        setTimeout(() => connectLobbyStream(), 50);
    } else {
        disconnectLobbyStream();
    }
});

/**
 * Creates a new lobby.
 * @param {boolean} isPrivate - Whether the lobby should be private.
 * @param {string} gameId - The ID of the game for this lobby.
 * @param {string[]} [whitelist] - An optional list of public keys for the whitelist (if private).
 * @returns {Promise<object>} The created lobby object.
 */
export async function createLobby(isPrivate, gameId, whitelist = []) {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const body = {
        is_private: isPrivate,
        game_id: gameId,
        whitelist: isPrivate ? whitelist : undefined,
    };

    const response = await fetch(`${apiBaseUrlValue}/lobbies`, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
            'Authorization': `Bearer ${token}`,
        },
        body: JSON.stringify(body),
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to create lobby: ${error}`);
    }

    // Refresh the lobby list after creating a new one
    await getLobbies();

    return await response.json();
}

/**
 * Joins a lobby.
 * @param {string} lobbyId - The ID of the lobby to join.
 */
export async function joinLobby(lobbyId) {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const response = await fetch(`${apiBaseUrlValue}/lobbies/${lobbyId}/join`, {
        method: 'POST',
        headers: { 'Authorization': `Bearer ${token}` },
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to join lobby: ${error}`);
    }

    // Refresh the lobby list to show the player in the lobby
    await getLobbies();
}

/**
 * Deletes a lobby (if owner) or leaves it (if member).
 * @param {string} lobbyId - The ID of the lobby to delete/leave.
 */
export async function deleteLobby(lobbyId) {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const response = await fetch(`${apiBaseUrlValue}/lobbies/${lobbyId}`, {
        method: 'DELETE',
        headers: { 'Authorization': `Bearer ${token}` },
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to delete/leave lobby: ${error}`);
    }

    // Refresh the lobby list
    await getLobbies();
}

/**
 * Starts a lobby (marks as InProgress). Only the owner can start.
 * This is useful when starting a game without WebSocket signaling (e.g., single player).
 * @param {string} lobbyId - The ID of the lobby to start.
 */
export async function startLobby(lobbyId) {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const response = await fetch(`${apiBaseUrlValue}/lobbies/${lobbyId}/start`, {
        method: 'POST',
        headers: { 'Authorization': `Bearer ${token}` },
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to start lobby: ${error}`);
    }
}

/**
 * Invites players to a private lobby by adding them to the whitelist.
 * Only the lobby owner can invite players.
 * @param {string} lobbyId - The ID of the lobby.
 * @param {string[]} playerPublicKeys - Array of public keys to invite.
 */
export async function inviteToLobby(lobbyId, playerPublicKeys) {
    const token = get(jwt);
    if (!token) throw new Error('Not logged in');

    const response = await fetch(`${apiBaseUrlValue}/lobbies/${lobbyId}/invite`, {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json',
            'Authorization': `Bearer ${token}`,
        },
        body: JSON.stringify({
            player_public_keys: playerPublicKeys,
        }),
    });

    if (!response.ok) {
        const error = await response.text();
        throw new Error(`Failed to invite players: ${error}`);
    }

    // Refresh the lobby list to show updated whitelist
    await getLobbies();
    
    return await response.json();
}


/**
 * Allows changing the Allumette server URL.
 * @param {string} newUrl - The new URL for the Allumette server.
 */
export function setApiUrl(newUrl) {
    API_BASE_URL.set(newUrl);
}

// Use dynamic import for argon2-browser and fallback to global/window if needed
let argon2;
// Export a promise that resolves when argon2 is ready
export const argon2Ready = browser ? (async () => {
  try {
    const argon2Module = await import('argon2-browser');
    argon2 = argon2Module.ArgonType ? argon2Module : (typeof window !== 'undefined' ? window.argon2 : undefined);
    if (!argon2) {
      throw new Error('argon2-browser module not found.');
    }
  } catch (e) {
    if (typeof window !== 'undefined') {
      argon2 = window.argon2;
    }
    if (!argon2) {
      console.error('argon2-browser could not be loaded:', e);
      throw e; // Propagate error to fail promise
    }
  }
})() : Promise.resolve();
