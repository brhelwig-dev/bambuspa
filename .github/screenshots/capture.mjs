// Renders the page in each state the printer can be in and writes a PNG per
// state.
//
// The states are driven by answering the printer's status in the browser rather
// than by pretending at the printer, so every screenshot is the real page and
// the real script reacting to a status payload it cannot tell from a live one.
import { chromium } from "playwright";
import { mkdir, writeFile } from "node:fs/promises";

const BASE = process.env.BASE || "http://127.0.0.1:8081";
const OUT = process.env.OUT || "screenshots";

// A phone, which is what this page is for.
const VIEWPORT = { width: 390, height: 844 };

const idle = {
  connected: true,
  gcodeState: "IDLE",
  actionsAllowed: true,
  bedTemp: 22.4,
  bedTarget: 0,
  nozzleTemp: 23.1,
  nozzleTarget: 0,
  chamberLight: true,
  fans: { cooling: 0, aux: 0, chamber: 0 },
  printActions: { pause: false, resume: false, stop: false },
  bedOffIn: null,
  nozzleOffIn: null,
  lampOffIn: 27180,
  ams: null,
  hms: null,
};

const ams = {
  tray_now: "1",
  ams: [{
    id: "0",
    humidity: "4",
    tray: [
      { id: "0", tray_type: "PLA", tray_color: "FF6B35FF", nozzle_temp_min: "190", nozzle_temp_max: "230", tray_info_idx: "GFA00" },
      { id: "1", tray_type: "PETG", tray_color: "2E86ABFF", nozzle_temp_min: "220", nozzle_temp_max: "260", tray_info_idx: "GFG00" },
      { id: "2", tray_type: "ABS", tray_color: "111418FF", nozzle_temp_min: "240", nozzle_temp_max: "270", tray_info_idx: "GFB00" },
      { id: "3", tray_type: "PLA", tray_color: "7BD88FFF", nozzle_temp_min: "190", nozzle_temp_max: "230", tray_info_idx: "GFA00" },
    ],
  }],
};

const printing = {
  ...idle,
  gcodeState: "RUNNING",
  actionsAllowed: false,
  bedTemp: 59.8,
  bedTarget: 60,
  nozzleTemp: 219.4,
  nozzleTarget: 220,
  progress: 47,
  jobName: "Minimalist_Pencil_Holder.3mf",
  layerNum: 88,
  totalLayerNum: 187,
  remainingMinutes: 132,
  fans: { cooling: 85, aux: 40, chamber: 0 },
  printActions: { pause: true, resume: false, stop: true },
  ams,
};

const states = [
  {
    name: "01-idle",
    title: "Idle",
    note: "Nothing printing. Every manual action is available.",
    status: { ...idle, ams },
  },
  {
    name: "02-printing",
    title: "Printing",
    note: "Mid-print: progress, layer and time remaining appear, and the bed and filament controls are refused.",
    status: printing,
  },
  {
    name: "03-paused",
    title: "Paused",
    note: "A paused print is still the same print — Resume replaces Pause, and it is not reported as ended.",
    status: {
      ...printing,
      gcodeState: "PAUSE",
      printActions: { pause: false, resume: true, stop: true },
    },
  },
  {
    name: "04-finished",
    title: "Finished",
    note: "The print completed. Actions are allowed again, so filament can be unloaded.",
    status: {
      ...idle, ams,
      gcodeState: "FINISH",
      jobName: "Minimalist_Pencil_Holder.3mf",
      bedTemp: 41.2,
      nozzleTemp: 132.6,
    },
  },
  {
    name: "05-ended-without-finishing",
    title: "Ended without finishing",
    note: "Stopped by hand, or the printer gave up — it reports the same state either way, so the page does not claim to know which.",
    status: { ...idle, ams, gcodeState: "FAILED", jobName: "Minimalist_Pencil_Holder.3mf" },
  },
  {
    name: "06-printer-error",
    title: "Printer error",
    note: "An alert the printer raised. Filament runout arrives this way rather than as a field of its own.",
    status: {
      ...printing,
      hms: [{ code: "0300-8000-0003-0002", message: "AMS filament runout" }],
    },
  },
  {
    name: "07-drying",
    title: "Bed drying",
    note: "The bed heated with nothing printing, showing the safety shut-off counting down.",
    status: {
      ...idle, ams,
      bedTemp: 58.9,
      bedTarget: 60,
      bedOffIn: 81900,
    },
  },
  {
    name: "08-disconnected",
    title: "Disconnected",
    note: "The printer is unreachable, and the Connection row says why. Every action is refused rather than sent into the dark.",
    status: {
      ...idle,
      connected: false,
      configured: true,
      problem: "No answer from 192.0.2.10. Check the address, and that the printer is on and on this network.",
      actionsAllowed: false,
      chamberLight: null,
      lampOffIn: null,
    },
  },
];

// Answers the printer's status with `status`, however the page asks for it: the
// live socket's status messages are swapped for it on their way to the page
// (the rest — camera range, recent prints — come from the real server), and
// /api/status is answered too, for a page that still polls. `printers`, when
// given, likewise replaces the list of printers the switcher shows.
async function stubStatus(page, status, printers = null) {
  await page.route("**/api/status", route => route.fulfill({ json: status }));
  await page.routeWebSocket("**/api/live", ws => {
    const server = ws.connectToServer();
    server.onMessage(message => {
      let type = null;
      try { type = JSON.parse(message).type; } catch {}
      if (type === "status") ws.send(JSON.stringify({ type: "status", ...status }));
      else if (type === "printers" && printers) ws.send(JSON.stringify({ type: "printers", printers }));
      else ws.send(message);
    });
  });
}

async function main() {
  await mkdir(OUT, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: VIEWPORT,
    deviceScaleFactor: 2,
    colorScheme: "dark",
  });
  // Headless Chromium refuses notifications outright, so left alone every
  // capture would show the card blocked — the capture browser's answer, not the
  // app's. These stubs stand in for the browser's own notification support so
  // the card renders as a phone renders it. Only the browser is stood in for;
  // the card's own logic is the real one.
  const notificationSupport = (permission, subscribed) => `
    window.Notification = { permission: ${JSON.stringify(permission)},
      requestPermission: async () => ${JSON.stringify(permission)} };
    window.PushManager = class {};
    // The subscription has to carry the server's own key, or the page correctly
    // treats it as bound to an identity that is gone and resets itself to Off.
    // Plain string replacement, not regexes: this whole stub is a template
    // literal, which eats the backslash out of an escape and leaves an invalid
    // pattern behind.
    const b64 = bytes => btoa(String.fromCharCode(...new Uint8Array(bytes)))
      .replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
    const standIn = async () => {
      const { key } = await (await fetch("/api/push/key")).json();
      const raw = atob(key.replace(/-/g, "+").replace(/_/g, "/"));
      // Real keys, because the server stores the subscription before the
      // settings it belongs to can be shown, and it refuses a malformed one.
      const pair = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
      const p256dh = b64(await crypto.subtle.exportKey("raw", pair.publicKey));
      const auth = b64(crypto.getRandomValues(new Uint8Array(16)));
      const endpoint = "https://push.example.net/stand-in";
      return {
        endpoint,
        options: { applicationServerKey: Uint8Array.from(raw, c => c.charCodeAt(0)).buffer },
        unsubscribe: async () => true,
        toJSON: () => ({ endpoint, keys: { p256dh, auth } }),
      };
    };
    Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: {
      register: async () => ({ pushManager: {
        getSubscription: async () => ${subscribed ? "standIn()" : "null"},
        subscribe: standIn,
      }}),
    }});
  `;

  const shots = [];
  for (const state of states) {
    const page = await context.newPage();
    // Answer the status ourselves; everything else reaches the real server, so
    // the camera card shows its genuine empty state rather than a staged one.
    await stubStatus(page, state.status);
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.waitForTimeout(400);
    const file = `${OUT}/${state.name}.png`;
    await page.screenshot({ path: file, fullPage: true });
    shots.push({ ...state, file });
    console.log(`captured ${state.title}`);
    await page.close();
  }

  // The settings screen, reached the way a person reaches it, with notifications
  // in each of the states a phone can show. A branch that predates the settings
  // screen simply has no gear, and capturing the rest is still worth doing.
  const settingsShots = [
    { name: "09-settings", title: "Settings", permission: "default", subscribed: false,
      note: "Its own screen behind the gear, not more cards under the printer status." },
    { name: "10-notifications-on", title: "Notifications on", permission: "granted", subscribed: true,
      note: "Once a phone has subscribed: which notifications this device wants, how often to be reminded the bed is on, and the test that proves delivery before anything rides on it." },
  ];
  for (const shot of settingsShots) {
    const page = await context.newPage();
    await page.addInitScript(notificationSupport(shot.permission, shot.subscribed));
    await stubStatus(page, { ...idle, ams });
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    if (!await page.locator("#settingsBtn").count()) {
      console.log("no settings screen on this branch, skipping");
      await page.close();
      break;
    }
    await page.click("#settingsBtn");
    await page.waitForTimeout(700);
    const file = `${OUT}/${shot.name}.png`;
    await page.screenshot({ path: file, fullPage: true });
    shots.push({ ...shot, file });
    console.log(`captured ${shot.title}`);
    await page.close();
  }

  // The events screen, with a spread of traffic: a command still waiting, one
  // acknowledged, one that never was, a raw printer report, and a notification.
  {
    const t = (mins) => new Date(Date.parse("2026-07-30T14:00:00Z") - mins * 60000).toISOString();
    const events = [
      { id: 5, at: t(0), kind: "notification", summary: "Print finished → 2 of 2 devices",
        payload: "Minimalist_Pencil_Holder.3mf", acked: t(0) },
      { id: 4, at: t(1), kind: "command", summary: "stop",
        payload: '{"print":{"sequence_id":"7","command":"stop"}}' },
      { id: 3, at: t(2), kind: "command", summary: "gcode M140 S60",
        payload: '{"print":{"sequence_id":"6","command":"gcode_line","param":"M140 S60\\n"}}',
        acked: t(2) },
      { id: 2, at: t(3), kind: "command", summary: "pause",
        payload: '{"print":{"sequence_id":"5","command":"pause"}}', error: "no acknowledgement" },
      { id: 1, at: t(4), kind: "report", summary: "report",
        payload: '{"print":{"gcode_state":"RUNNING","bed_temper":59.8,"mc_percent":47}}' },
    ];
    const page = await context.newPage();
    await stubStatus(page, { ...idle, ams });
    await page.route("**/api/events", route => route.fulfill({ json: { events } }));
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    if (await page.locator("#eventsBtn").count()) {
      await page.click("#eventsBtn");
      // Open the raw view on the waiting command, since being able to read the
      // message is the point of keeping it.
      await page.locator(".event details").first().evaluate(d => d.open = true);
      await page.waitForTimeout(400);
      const file = `${OUT}/11-events.png`;
      await page.screenshot({ path: file, fullPage: true });
      shots.push({
        name: "11-events", title: "Events", file,
        note: "What went to the printer, what came back, and what was sent to a phone — with the raw message behind each.",
      });
      console.log("captured Events");
    }
    await page.close();
  }

  // Light theme, and a dashboard with sections hidden and reordered.
  {
    const page = await context.newPage();
    await page.addInitScript(`try { localStorage.setItem("theme", "light"); } catch {}`);
    await stubStatus(page, printing);
    await page.route("**/api/settings", route => route.fulfill({
      json: { "camera-storage": 1024, "bed-off-after": 86400,
              "nozzle-off-after": 900, "lamp-off-after": 28800,
              dashboard: "machineCard,jobCard,controlsCard" },
    }));
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.waitForTimeout(600);
    const file = `${OUT}/12-light-rearranged.png`;
    await page.screenshot({ path: file, fullPage: true });
    shots.push({
      name: "12-light-rearranged", title: "Light theme, rearranged", file,
      note: "The light theme, with the dashboard reordered and the camera, drying, nozzle and filament sections hidden.",
    });
    console.log("captured Light theme");
    await page.close();
  }

  // Two printers: the switcher above the dashboard.
  {
    const page = await context.newPage();
    await stubStatus(page, printing, [
      { id: 1, name: "Garage", connected: true, gcodeState: "RUNNING", progress: 42 },
      { id: 2, name: "Office", connected: false, problem: "No answer from 192.0.2.11.", gcodeState: "unknown", progress: null },
    ]);
    await page.goto(BASE + "/", { waitUntil: "networkidle" });
    await page.waitForTimeout(600);
    const file = `${OUT}/13-two-printers.png`;
    await page.screenshot({ path: file });
    shots.push({
      name: "13-two-printers", title: "Two printers", file,
      note: "The switcher above the dashboard, with one printer printing and one that can't be reached.",
    });
    console.log("captured Two printers");
    await page.close();
  }

  await browser.close();
  // What was captured and what each shot is, for the comparison and the comment.
  const manifest = JSON.stringify(shots.map(s => ({ name: s.name, title: s.title, note: s.note })), null, 2);
  await writeFile(`${OUT}/shots.json`, manifest + "\n");
  console.log(manifest);
}

await main();
