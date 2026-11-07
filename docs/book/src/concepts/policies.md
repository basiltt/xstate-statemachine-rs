# Policies

The chart root (or `MachineBuilder`) carries the runtime policies. Defaults follow XState v5:

| Policy | Meaning | Default |
|---|---|---|
| `strict` | undeclared events are an error instead of ignored | `false` |
| `strictTargets` | refuse the deprecated sibling fallback for `.child` targets | `false` |
| `strictConfig` | unknown config keys are errors instead of warnings | `false` |
| `maxIterations` | eventless (`always`) / self-generated event budget per macrostep | 1000 |
| `actionErrorPolicy` | `continue` (log, skip), `rollback` (restore context), or `fail` (move to `error`) | `fail` |
| `guardErrorPolicy` | `false`, `true`, or `raise` for a guard that errors | `false` |
| `onUnhandled` | `ignore` (drop), `defer` (bounded queue), or `error` | `ignore` |

`actionErrorPolicy: "rollback"` needs a cloneable context. The Python library's default
`actionErrorPolicy` was `continue`; this library follows XState's `fail`, so set the policy
explicitly when migrating charts (see the [migration guide](../guide/migrating-from-python.md)).
