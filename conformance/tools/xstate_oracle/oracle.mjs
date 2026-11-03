// Conformance oracle: runs cases through the real XState v5 engine and prints what it observed.
//
// Usage:  node oracle.mjs <case.json | cases-dir>...
// Output: one JSON document on stdout:
//   { "oracle": {"name": "xstate", "version": "5.x"}, "cases": { "<name>": Expectation } }
//
// The case format is documented in ../../README.md. Guards are constants (recorded by name in
// evaluation order when the case sets `logic.record_guards`), user actions are recorded by name, services complete with a fixed result (or a fixed error), and named
// delays are constants — the same stubbing the Python oracle applies, so the two outputs are
// directly comparable.
//
// XState's JSON only knows *named* guards and actions; composition and built-ins are written
// in code (`raise(...)`, `and([...])`, `stateIn(...)`). The Python/Rust JSON dialect spells
// those as `{type, params}` objects. `translate()` rewrites the dialect into the native XState
// creators before the machine is created — exactly what a Stately user would write by hand.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import {
  SimulatedClock,
  and,
  assign,
  cancel,
  createActor,
  createMachine,
  emit,
  enqueueActions,
  fromPromise,
  log,
  not,
  or,
  raise,
  sendParent,
  sendTo,
  stateIn,
  stopChild,
} from "xstate";

const require = createRequire(import.meta.url);
const XSTATE_VERSION = require("xstate/package.json").version;

// Built-in action names in every spelling the dialect accepts → canonical name.
const BUILTIN = new Map(Object.entries({
  assign: "assign", "xstate.assign": "assign",
  raise: "raise", raise_: "raise", "xstate.raise": "raise",
  sendTo: "sendTo", send_to: "sendTo", "xstate.sendTo": "sendTo",
  sendParent: "sendParent", send_parent: "sendParent", "xstate.sendParent": "sendParent",
  forwardTo: "forwardTo", forward_to: "forwardTo", "xstate.forwardTo": "forwardTo",
  escalate: "escalate", "xstate.escalate": "escalate",
  log: "log", "xstate.log": "log",
  cancel: "cancel", "xstate.cancel": "cancel",
  stopChild: "stopChild", stop_child: "stopChild", stop: "stopChild", "xstate.stopChild": "stopChild",
  spawnChild: "spawnChild", "xstate.spawnChild": "spawnChild",
  emit: "emit", "xstate.emit": "emit",
  choose: "choose", "xstate.choose": "choose",
}));

function* walk(node) {
  if (Array.isArray(node)) { for (const n of node) yield* walk(n); return; }
  if (node && typeof node === "object") { yield node; for (const v of Object.values(node)) yield* walk(v); }
}

const asList = (v) => (v === undefined || v === null ? [] : Array.isArray(v) ? v : [v]);
const eventOf = (v) => (typeof v === "string" ? { type: v } : v);

// ----------------------------------------------------------------------------- translation

function translateGuard(g) {
  if (typeof g === "string") return g.startsWith("!") ? not(g.slice(1)) : g;
  if (!g || typeof g !== "object") return g;
  const operands = g.children ?? g.params?.guards ?? g.params?.children ?? [];
  switch (g.type) {
    case "and": return and(operands.map(translateGuard));
    case "or": return or(operands.map(translateGuard));
    case "not": return not(translateGuard(g.params?.guard ?? operands[0]));
    case "stateIn": return stateIn(typeof g.params === "string" ? g.params : (g.params?.stateValue ?? g.params?.state));
    default: return g;
  }
}

function translateAction(a) {
  if (typeof a === "string") return a;
  if (!a || typeof a !== "object") return a;
  const kind = BUILTIN.get(a.type);
  if (!kind) return a;
  const p = a.params ?? {};
  const opts = { delay: p.delay, id: p.id };
  switch (kind) {
    case "assign": return assign(p.assignment ?? p);
    case "raise": return raise(eventOf(p.event), opts);
    case "sendTo": return (p.to === "parent" || p.to === "_parent") ? sendParent(eventOf(p.event), opts) : sendTo(p.to, eventOf(p.event), opts);
    case "sendParent": return sendParent(eventOf(p.event), opts);
    case "forwardTo": return sendTo(p.to, ({ event }) => event);
    case "escalate": return sendParent({ type: "xstate.error", error: p.error });
    case "log": return log(p.expr, p.label);
    case "cancel": return cancel(p.sendId);
    case "stopChild": return stopChild(p.id);
    case "spawnChild": return a; // not exercised by the fixtures yet
    case "emit": return emit(eventOf(p.event));
    case "choose": {
      const branches = (p.branches ?? []).map((b) => ({
        guard: b.guard ?? b.cond ? translateGuard(b.guard ?? b.cond) : undefined,
        actions: asList(b.actions).map(translateAction),
      }));
      return enqueueActions(({ enqueue, check }) => {
        for (const branch of branches) {
          if (!branch.guard || check(branch.guard)) {
            for (const act of branch.actions) enqueue(act);
            return;
          }
        }
      });
    }
    default: return a;
  }
}

function translateTransition(t) {
  if (t === null) return {}; // forbidden transition: XState's empty targetless transition
  if (typeof t === "string") return t;
  if (Array.isArray(t)) return t.map(translateTransition);
  const out = { ...t };
  if ("actions" in out) out.actions = asList(out.actions).map(translateAction);
  if ("guard" in out) out.guard = translateGuard(out.guard);
  if ("cond" in out) { out.guard = translateGuard(out.cond); delete out.cond; }
  return out;
}

function translateState(s) {
  const out = { ...s };
  for (const key of ["entry", "exit"]) if (key in out) out[key] = asList(out[key]).map(translateAction);
  for (const key of ["on", "after"]) {
    if (out[key] && typeof out[key] === "object") {
      out[key] = Object.fromEntries(Object.entries(out[key]).map(([k, v]) => [k, translateTransition(v)]));
    }
  }
  if ("always" in out) out.always = translateTransition(out.always);
  if ("onDone" in out) out.onDone = translateTransition(out.onDone);
  if (out.invoke) {
    out.invoke = asList(out.invoke).map((inv) => {
      const i = { ...inv };
      if ("onDone" in i) i.onDone = translateTransition(i.onDone);
      if ("onError" in i) i.onError = translateTransition(i.onError);
      return i;
    });
  }
  if (out.states) out.states = Object.fromEntries(Object.entries(out.states).map(([k, v]) => [k, translateState(v)]));
  return out;
}

function translate(config) {
  return translateState(structuredClone(config));
}

// ----------------------------------------------------------------------------- names

function collectNames(config) {
  const guards = new Set(), actions = new Set(), services = new Set(), delays = new Set();
  const addGuard = (g) => {
    if (typeof g === "string") { guards.add(g.startsWith("!") ? g.slice(1) : g); return; }
    if (g && typeof g === "object") {
      if (["and", "or", "not"].includes(g.type)) {
        for (const c of g.children ?? g.params?.guards ?? g.params?.children ?? []) addGuard(c);
        if (g.params?.guard) addGuard(g.params.guard);
      } else if (g.type && g.type !== "stateIn") guards.add(g.type);
    }
  };
  const addAction = (a) => {
    for (const x of asList(a)) {
      if (typeof x === "string") actions.add(x);
      else if (x && typeof x === "object" && typeof x.type === "string") {
        if (BUILTIN.get(x.type) === "choose") {
          for (const b of x.params?.branches ?? []) { addAction(b.actions); if (b.guard ?? b.cond) addGuard(b.guard ?? b.cond); }
        } else if (!BUILTIN.has(x.type)) actions.add(x.type);
      }
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

// ----------------------------------------------------------------------------- running

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

async function runCase(c) {
  const logic = c.logic ?? {};
  const names = collectNames(c.machine);
  const ran = [];
  const guardLog = [];
  const recordGuards = logic.record_guards === true;

  const guards = {};
  for (const g of names.guards) {
    guards[g] = () => {
      if (recordGuards) guardLog.push(g);
      return (logic.guards?.[g] ?? true) === true;
    };
  }
  const actions = {};
  for (const a of names.actions) actions[a] = () => {};
  const actors = {};
  for (const s of names.services) {
    const spec = logic.services?.[s] ?? { result: null };
    actors[s] = fromPromise(async () => {
      if (spec.error !== undefined) throw new Error(String(spec.error));
      return spec.result ?? null;
    });
  }
  const delays = { ...(logic.delays ?? {}) };

  const machine = createMachine(translate(c.machine)).provide({ guards, actions, actors, delays });
  const clock = new SimulatedClock();
  const actor = createActor(machine, {
    clock,
    inspect: (ev) => {
      if (ev.type === "@xstate.action" && ev.actorRef === actor) {
        const t = ev.action?.type;
        if (typeof t === "string" && names.actions.has(t)) ran.push(t);
      }
    },
  });
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
    if (recordGuards) out.guards = guardLog.splice(0, guardLog.length);
    return out;
  };

  actor.start();
  await flush();
  const initial = observe();
  const steps = [];
  for (const step of c.steps ?? []) {
    if ("send" in step) actor.send(eventOf(step.send));
    else if ("send_batch" in step) for (const ev of step.send_batch) actor.send(eventOf(ev));
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
