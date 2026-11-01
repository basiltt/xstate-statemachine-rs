// Conformance oracle: runs cases through the real XState v5 engine and prints what it observed.
//
// Usage:  node oracle.mjs <case.json | cases-dir>...
// Output: one JSON document on stdout:
//   { "oracle": {"name": "xstate", "version": "5.x"}, "cases": { "<name>": Expectation } }
//
// The case format is documented in ../../README.md. Guards are constants, user actions are
// recorded by name, services complete with a fixed result (or a fixed error), and named
// delays are constants — the same stubbing the Python oracle applies, so the two outputs are
// directly comparable.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import {
  SimulatedClock,
  assign,
  cancel,
  createActor,
  createMachine,
  emit,
  fromPromise,
  log,
  raise,
  sendParent,
  sendTo,
} from "xstate";

const require = createRequire(import.meta.url);
const XSTATE_VERSION = require("xstate/package.json").version;

// Built-in action names the Python engine recognises in JSON form. They are provided as
// XState built-ins here and excluded from the recorded user-action list on both sides.
const BUILTIN_ACTIONS = new Set([
  "assign", "raise", "log", "sendTo", "sendParent", "forwardTo", "escalate", "cancel",
  "stopChild", "stop", "spawnChild", "emit", "pure", "choose", "enqueueActions",
]);

function* walk(node) {
  if (Array.isArray(node)) { for (const n of node) yield* walk(n); return; }
  if (node && typeof node === "object") { yield node; for (const v of Object.values(node)) yield* walk(v); }
}

function collectNames(config) {
  const guards = new Set(), actions = new Set(), services = new Set(), delays = new Set();
  const addGuard = (g) => {
    if (typeof g === "string") { if (g.startsWith("!")) guards.add(g.slice(1)); else guards.add(g); return; }
    if (g && typeof g === "object") {
      if (["and", "or", "not"].includes(g.type)) {
        for (const c of g.children ?? g.params?.guards ?? g.params?.children ?? []) addGuard(c);
        if (g.params?.guard) addGuard(g.params.guard);
      } else if (g.type && g.type !== "stateIn") guards.add(g.type);
    }
  };
  const addAction = (a) => {
    const list = Array.isArray(a) ? a : [a];
    for (const x of list) {
      if (typeof x === "string") actions.add(x);
      else if (x && typeof x === "object" && typeof x.type === "string") actions.add(x.type);
    }
  };
  for (const obj of walk(config)) {
    if ("guard" in obj) addGuard(obj.guard);
    if ("cond" in obj) addGuard(obj.cond);
    if ("actions" in obj) addAction(obj.actions);
    if ("entry" in obj) addAction(obj.entry);
    if ("exit" in obj) addAction(obj.exit);
    if ("src" in obj && typeof obj.src === "string") services.add(obj.src);
    if ("after" in obj && obj.after && typeof obj.after === "object") {
      for (const k of Object.keys(obj.after)) if (!/^\d+(\.\d+)?$/.test(k)) delays.add(k);
    }
  }
  return { guards, actions, services, delays };
}

// The Python/Rust JSON dialect spells a forbidden transition as `"E": null`. XState v5 has no
// JSON spelling for it (`undefined` is not JSON); its equivalent is the empty targetless
// transition `{}`, which consumes the event without doing anything and blocks ancestors.
function translateForbidden(config) {
  for (const obj of walk(config)) {
    if (obj.on && typeof obj.on === "object" && !Array.isArray(obj.on)) {
      for (const [k, v] of Object.entries(obj.on)) if (v === null) obj.on[k] = {};
    }
  }
  return config;
}

function leafStateIds(snapshot) {
  const nodes = snapshot._nodes ?? [];
  const ids = new Set(nodes.map((n) => n.id));
  const hasActiveChild = (n) => Object.values(n.states ?? {}).some((c) => ids.has(c.id));
  return nodes.filter((n) => !hasActiveChild(n)).map((n) => n.id).sort();
}

const STATUS = { active: "running", done: "done", error: "error", stopped: "stopped" };

async function flush() {
  for (let i = 0; i < 20; i += 1) await new Promise((r) => setImmediate(r));
}

function normaliseEvent(step) {
  const s = step.send;
  if (typeof s === "string") return { type: s };
  return { ...s };
}

async function runCase(c) {
  const logic = c.logic ?? {};
  const names = collectNames(c.machine);
  const ran = [];

  const guards = {};
  for (const g of names.guards) guards[g] = () => (logic.guards?.[g] ?? true) === true;

  const actions = {
    assign: assign((_, params) => params?.assignment ?? {}),
    raise: raise((_, params) => params?.event ?? { type: params?.type }, { delay: (_, params) => params?.delay }),
    log: log(),
    sendTo: sendTo((_, params) => params?.to, (_, params) => params?.event),
    sendParent: sendParent((_, params) => params?.event),
    cancel: cancel((_, params) => params?.sendId),
    emit: emit((_, params) => params?.event),
  };
  for (const a of names.actions) if (!BUILTIN_ACTIONS.has(a)) actions[a] = () => {};

  const actors = {};
  for (const s of names.services) {
    const spec = logic.services?.[s] ?? { result: null };
    actors[s] = fromPromise(async () => {
      if (spec.error !== undefined) throw new Error(String(spec.error));
      return spec.result ?? null;
    });
  }

  const delays = { ...(logic.delays ?? {}) };

  const config = translateForbidden(structuredClone(c.machine));
  const machine = createMachine(config).provide({ guards, actions, actors, delays });
  const clock = new SimulatedClock();
  const actor = createActor(machine, {
    clock,
    inspect: (ev) => {
      if (ev.type === "@xstate.action" && ev.actorRef === actor) {
        const t = ev.action?.type;
        if (typeof t === "string" && !BUILTIN_ACTIONS.has(t) && !t.startsWith("xstate.")) ran.push(t);
      }
    },
  });
  // Swallow unhandled error-snapshot rejections so a case that drives the machine into
  // `error` does not crash the oracle.
  actor.subscribe({ error: () => {} });

  const observe = () => {
    const snap = actor.getSnapshot();
    const out = {
      state_ids: leafStateIds(snap),
      context: snap.context ?? {},
      actions: ran.splice(0, ran.length),
      status: STATUS[snap.status] ?? snap.status,
    };
    if (snap.status === "done") out.output = snap.output ?? null;
    if (snap.status === "error") out.error = String(snap.error?.message ?? snap.error ?? "");
    return out;
  };

  actor.start();
  await flush();
  const initial = observe();
  const steps = [];
  for (const step of c.steps ?? []) {
    if ("send" in step) actor.send(normaliseEvent(step));
    else if ("advance_ms" in step) clock.increment(step.advance_ms);
    else throw new Error(`unknown step ${JSON.stringify(step)}`);
    await flush();
    steps.push(observe());
  }
  actor.stop();
  return { initial, steps };
}

function loadCases(paths) {
  const files = [];
  for (const p of paths) {
    if (statSync(p).isDirectory()) {
      for (const f of readdirSync(p).sort()) if (f.endsWith(".json")) files.push(join(p, f));
    } else files.push(p);
  }
  return files.map((f) => JSON.parse(readFileSync(f, "utf8")));
}

const cases = loadCases(process.argv.slice(2));
const out = { oracle: { name: "xstate", version: XSTATE_VERSION }, cases: {} };
for (const c of cases) {
  try {
    out.cases[c.name] = await runCase(c);
  } catch (err) {
    out.cases[c.name] = { error: String(err?.stack ?? err) };
  }
}
process.stdout.write(JSON.stringify(out, null, 2));
