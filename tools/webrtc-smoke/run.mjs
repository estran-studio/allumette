import assert from "node:assert/strict";
import { chromium } from "playwright-core";
import { AllumetteBrowserClient } from "../../packages/browser-client/index.js";

// Real encrypted data exchange with ephemeral credentials; never print secrets/SDP.
const origin =
  process.env.ALLUMETTE_ORIGIN || "https://allumette.estran.studio";
const transport = process.env.TURN_TRANSPORT;
assert(
  !transport || ["udp", "tcp"].includes(transport),
  "TURN_TRANSPORT must be udp or tcp",
);
const client = new AllumetteBrowserClient(origin);
let browser;
let pumping = false;
let pump;
const pages = [];
try {
  await client.loginGuest("webrtc-smoke");
  let iceServers = await client.request("/ice-servers");
  if (transport) {
    iceServers = iceServers
      .map((server) => ({
        ...server,
        urls: server.urls.filter(
          (url) =>
            url.startsWith("turn:") && url.includes(`transport=${transport}`),
        ),
      }))
      .filter((server) => server.urls.length);
  }
  assert(
    iceServers.some((server) => server.username && server.credential),
    "Authenticated TURN required",
  );
  browser = await chromium.launch({
    executablePath: process.env.CHROMIUM_EXECUTABLE || "/usr/bin/chromium",
    headless: true,
    args: process.env.CHROMIUM_NO_SANDBOX === "1" ? ["--no-sandbox"] : [],
  });
  for (let i = 0; i < 2; i++) {
    const page = await (await browser.newContext()).newPage();
    pages.push(page);
    await page.goto(`${origin}/health`);
    await page.evaluate((iceServers) => {
      window.pc = new RTCPeerConnection({
        iceServers,
        iceTransportPolicy: "relay",
      });
      window.candidates = [];
      window.received = [];
      window.iceErrors = [];
      pc.onicecandidate = (event) => {
        if (event.candidate) candidates.push(event.candidate.toJSON());
      };
      pc.onicecandidateerror = (event) => iceErrors.push(event.errorCode);
      pc.ondatachannel = (event) => {
        window.dc = event.channel;
        dc.onmessage = (event) => received.push(event.data);
      };
    }, iceServers);
  }
  const [a, b] = pages;
  await a.evaluate(async () => {
    window.dc = pc.createDataChannel("proof");
    await pc.setLocalDescription(await pc.createOffer());
  });
  const offer = await a.evaluate(() => pc.localDescription.toJSON());
  await b.evaluate(async (offer) => {
    await pc.setRemoteDescription(offer);
    await pc.setLocalDescription(await pc.createAnswer());
  }, offer);
  const answer = await b.evaluate(() => pc.localDescription.toJSON());
  await a.evaluate((answer) => pc.setRemoteDescription(answer), answer);
  // Trickle immediately, rather than waiting for gathering before the answer.
  pumping = true;
  pump = (async () => {
    while (pumping) {
      for (const [from, to] of [
        [a, b],
        [b, a],
      ]) {
        const candidates = await from.evaluate(() =>
          window.candidates.splice(0),
        );
        for (const candidate of candidates)
          await to.evaluate(
            (candidate) => pc.addIceCandidate(candidate),
            candidate,
          );
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  })();
  await Promise.race([
    pump,
    Promise.all(
      pages.map((page) =>
        page.waitForFunction(
          () =>
            pc.connectionState === "connected" &&
            window.dc?.readyState === "open",
          {},
          { timeout: 30000 },
        ),
      ),
    ),
  ]);
  await a.evaluate(() => dc.send("allumette-relay-proof"));
  await b.waitForFunction(
    () => received.includes("allumette-relay-proof"),
    {},
    { timeout: 5000 },
  );
  await a.evaluate(() => {
    dc.onmessage = (event) => received.push(event.data);
  });
  await b.evaluate(() => dc.send("allumette-relay-ack"));
  await a.waitForFunction(
    () => received.includes("allumette-relay-ack"),
    {},
    { timeout: 5000 },
  );
  for (const page of pages) {
    const proof = await page.evaluate(async () => {
      const stats = await pc.getStats();
      const transport = [...stats.values()].find(
        (s) => s.type === "transport" && s.selectedCandidatePairId,
      );
      const pair = transport && stats.get(transport.selectedCandidatePairId);
      return {
        connection: pc.connectionState,
        localType: pair && stats.get(pair.localCandidateId)?.candidateType,
        remoteType: pair && stats.get(pair.remoteCandidateId)?.candidateType,
        bytesReceived: pair?.bytesReceived,
      };
    });
    assert.equal(proof.localType, "relay");
    assert.equal(proof.remoteType, "relay");
    assert(proof.bytesReceived > 0);
    console.log(JSON.stringify(proof));
  }
  console.log(
    `PASS: bidirectional TURN data channel (${transport || "all transports"})`,
  );
} catch (error) {
  console.error("FAIL:", error.message);
  for (const page of pages) {
    console.error(
      await page
        .evaluate(() => ({
          connection: pc?.connectionState,
          ice: pc?.iceConnectionState,
          gathering: pc?.iceGatheringState,
          errorCodes: window.iceErrors,
        }))
        .catch(() => ({ pageUnavailable: true })),
    );
  }
  process.exitCode = 1;
} finally {
  pumping = false;
  await pump?.catch(() => {});
  await browser?.close();
  client.logout();
}
