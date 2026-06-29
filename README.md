# Remote Execution Primitive — Service Specification

## Core Idea

Run any shell script on any registered device, from anywhere, with streaming logs,
as if it were running locally.

The invoker does not matter — a Tekton step, a GitHub Actions step, a bash script
on a laptop, a cron job, a button in a web UI. Anything that can make an HTTP POST
and read an SSE stream can dispatch a job to any registered device in the mesh.

The device does not matter either — a Mac mini under a desk, a Raspberry Pi
controlling hardware, a bare-metal GPU server, a developer's laptop. If it runs
`ginger-infra start --capability <tags>`, it is reachable.

---

## Why This Exists

Most remote execution solutions require:
- Static IPs or inbound connectivity
- SSH key management
- Per-CI-system runner agents (one for GitHub, one for Jenkins, one for GitLab...)
- Containerisation (which breaks native builds, hardware access, GUI tools)

This system requires none of that. The device connects outbound to the WAMP broker
once. From that moment it is reachable by any authorised caller, from anywhere,
with no network configuration.

---

## Real-World Use Cases

| Invoker | Device | Job |
|---|---|---|
| Tekton step | Mac mini (arm64) | Build native iOS / arm64 binary |
| GitHub Actions step | Linux workstation (GPU) | Run ML training job |
| Developer's laptop | Raspberry Pi | Toggle a GPIO pin, switch off an AC unit |
| Web UI button | Any registered machine | Run a deployment script |
| Cron job | Home server | Nightly backup |
| CI pipeline | Any capable device | Run hardware-in-the-loop tests |

The target device for CI use is a **Mac mini** — cheap, silent, fanless, runs 24/7,
native arm64, real Xcode toolchain. One machine can serve multiple teams
simultaneously. At $0.08/minute for hosted macOS runners, a Mac mini pays for
itself in roughly 125 hours of build time.

---

## System Overview

```
Any Invoker                  executor service               Device (any OS, any hardware)
    │                               │                                  │
    │── POST /run-job ─────────────>│                                  │
    │   {capability, script, env}   │                                  │
    │                               │── call("rpc_job", device) ──────>│
    │<── SSE stream starts ─────────│   (awaiting reply)               │
    │                               │                                  │ (script running)
    │                               │<── publish(stdout line) ─────────│
    │<── data: {line, stream} ──────│                                  │
    │                               │<── publish(stderr line) ─────────│
    │<── data: {line, stream} ──────│                                  │
    │                               │                                  │ (script exits)
    │                               │<────── Ok({exit_code}) ──────────│
    │<── data: {done, exit_code} ───│                                  │
    │                               │── call("cleanup_job", device) ──>│
    │ (exit 0 or 1)                 │                                  │ (cleanup runs)
```

---

## Components

### ginger-infra agent

Runs on the device. Registers with the presence service, heartbeats every 5 seconds,
exposes RPC handlers. One agent serves any number of concurrent callers and any
CI system simultaneously.

### Presence service

Tracks which devices are online and what they can do. Callers query it to find a
suitable device before dispatching a job.

### WAMP broker

The NAT traversal and authentication layer. Devices connect outbound — no inbound
ports, no firewall rules, no SSH keys needed.

### executor service

The HTTP/SSE adapter. Accepts job requests, finds a capable device, dispatches the
job via WAMP RPC, streams output back to the caller. This is where complexity lives
so that the caller stays simple.

### ginger-infra CLI

The caller-side tool. Reads script files, POSTs to the executor, streams SSE to
stdout, exits with the remote script's exit code. One command, works from any
environment.

---

## Script Conventions

Each remotely executable job is a pair of shell scripts:

```
.tekton/tasks/build-arm64/
    run.sh        # the job
    cleanup.sh    # always runs — even on failure or cancellation
```

Or anywhere else that makes sense for the invoker:

```
scripts/
    gpu-train/
        run.sh
        cleanup.sh

home-automation/
    ac-off/
        run.sh
        cleanup.sh
```

**run.sh** — the job. Exit 0 on success, non-zero on failure.

**cleanup.sh** — runs unconditionally after `run.sh` completes or the job is
cancelled. The device is not a container — there is no `docker rm`. Cleanup is
a first-class concern. It receives the same env vars as `run.sh`.

Scripts are `.sh` files committed to the repository — readable, diffable,
syntax-highlighted. The CLI reads them and sends their content in the POST body.

---

## API

### POST /run-job

**Request body (JSON):**
```json
{
  "capability": "osxarm64",
  "script": "#!/bin/bash\ncargo build --release --target aarch64-apple-darwin\n",
  "cleanup_script": "#!/bin/bash\nrm -rf ./target/aarch64-apple-darwin/release\n",
  "env": {
    "CARGO_HOME": "/Users/runner/.cargo",
    "PATH": "/usr/local/bin:/usr/bin"
  }
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| capability | string | yes | Target device capability e.g. `osxarm64`, `gpu`, `raspberry-pi` |
| script | string | yes | Content of `run.sh` |
| cleanup_script | string | yes | Content of `cleanup.sh` — always executed |
| env | object | no | Environment variables injected into both scripts |

**Response: `text/event-stream` (SSE)**

```
data: {"type":"log","stream":"stdout","line":"Compiling myapp v0.1.0"}

data: {"type":"log","stream":"stderr","line":"warning: unused variable"}

data: {"type":"done","exit_code":0}
```

| Event type | Fields | Description |
|---|---|---|
| `log` | `stream`, `line` | One line of output from the remote script |
| `done` | `exit_code` | Script finished. Cleanup dispatched. Stream closes. |
| `error` | `message` | Job failed to dispatch or device was lost. Stream closes. |

**Error responses (before stream starts):**

| Status | Reason |
|---|---|
| 503 | Service is draining — no new jobs accepted |
| 404 | No device found with requested capability |
| 500 | Internal error |

---

### GET /drain-status

```json
{
  "draining": false,
  "jobs_in_flight": 3
}
```

---

### GET /health

`200 OK` if WAMP client is connected and presence service is reachable, `503` otherwise.

---

## Internal Design

### JobRegistry

```rust
struct JobRegistry {
    jobs: Arc<Mutex<HashMap<String, mpsc::Sender<JobEvent>>>>,
    draining: Arc<AtomicBool>,
    in_flight: Arc<AtomicUsize>,  // includes in-progress cleanup tasks
}

enum JobEvent {
    Log { stream: String, line: String },
    Done { exit_code: i32 },
    Error { message: String },
}
```

### Request Lifecycle

```
POST /run-job
  │
  ├─ draining? → 503
  │
  ├─ GET presence/available-devices/by-capability?capability=osxarm64
  │   └─ empty? → 404
  │
  ├─ generate job_id
  ├─ create mpsc channel (tx → JobRegistry, rx → SSE stream)
  ├─ insert job_id into JobRegistry
  ├─ increment in_flight
  │
  ├─ spawn task:
  │     call("rpc_job", device_channel, {
  │       job_id, script, env,
  │       reply_channel: executor's WAMP channel
  │     })
  │     │
  │     ├─ Ok({exit_code}) → send JobEvent::Done
  │     └─ Err(e)          → send JobEvent::Error
  │     │
  │     └─ either way → dispatch cleanup_job RPC
  │                    → remove job_id from registry
  │                    → wait for cleanup to complete
  │                    → decrement in_flight
  │
  └─ SSE stream (reads from mpsc rx)
       ├─ yields log lines as they arrive
       ├─ closes on Done or Error
       └─ on client disconnect → cancel_job RPC → cleanup still runs
```

### Drain / Graceful Shutdown

On SIGTERM:
- Stop accepting new POST /run-job (return 503)
- Wait for `in_flight` to reach 0 — this includes cleanup scripts
- Exit 0

Kubernetes `terminationGracePeriodSeconds` should be set to the longest job
you would ever expect to run. In-flight jobs always finish on the same pod.
New jobs route to other replicas or the new pod once ready.

---

## Device-Side Handlers (ginger-infra agent)

### `rpc_job`

1. Write `script` to `/tmp/{job_id}/run.sh`, `chmod +x`
2. Spawn bash with script and env vars
3. Stream stdout/stderr lines to `reply_channel` as WAMP publish events
4. Wait for process to exit
5. Return `Ok({exit_code})` or `Err({exit_code, error})`
   — this return is what unblocks the executor's `call()` and triggers cleanup

### `cleanup_job`

1. Write `cleanup_script` to `/tmp/{job_id}/cleanup.sh`, `chmod +x`
2. Run with same env vars, wait for completion
3. Remove `/tmp/{job_id}/`
4. Return `Ok` regardless of exit code — cleanup failures are logged, never propagated

### `cancel_job`

1. Look up running process for `job_id`
2. SIGTERM → brief wait → SIGKILL if still alive
3. Return `Ok`

---

## CLI Usage

```bash
# from a Tekton step, GitHub Actions, a laptop — anywhere
ginger-infra rpc-job \
  --capability osxarm64 \
  --script     ./scripts/build/run.sh \
  --cleanup    ./scripts/build/cleanup.sh \
  --env-file   .envrc
```

The CLI:
1. Reads the `.sh` files from disk
2. POSTs to the executor service
3. Streams SSE events to stdout (caller sees live logs)
4. Exits with the `exit_code` from the `done` event

---

## Example Integrations

**Tekton:**
```yaml
steps:
  - name: build-arm64
    image: gingersociety/ginger-infra:latest
    script: |
      #!/bin/bash
      set -e
      ginger-infra rpc-job \
        --capability osxarm64 \
        --script     /workspace/source/.tekton/tasks/build-arm64/run.sh \
        --cleanup    /workspace/source/.tekton/tasks/build-arm64/cleanup.sh \
        --env-file   /workspace/source/.envrc
```

**GitHub Actions:**
```yaml
- name: Build arm64
  run: |
    ginger-infra rpc-job \
      --capability osxarm64 \
      --script     ./.github/scripts/build/run.sh \
      --cleanup    ./.github/scripts/build/cleanup.sh
```

**From a laptop:**
```bash
ginger-infra rpc-job \
  --capability raspberry-pi \
  --script     ./home/ac-off/run.sh \
  --cleanup    ./home/ac-off/cleanup.sh
```


---

## Failure Modes

| Failure | Behaviour |
|---|---|
| No capable device online | 404 before stream starts — caller fails immediately, no cleanup needed |
| Device goes offline mid-job | `call()` returns `callee_offline` → SSE error → cleanup cannot run → logged |
| Device times out | `call()` returns `callee_timeout` → SSE error → cleanup dispatched if device reachable |
| Script exits non-zero | `call()` returns `Err({exit_code})` → SSE error → cleanup dispatched → caller exits 1 |
| Cleanup script fails | Logged, ignored — never propagates to caller |
| Caller disconnects mid-stream | SSE broken pipe → `cancel_job` RPC → cleanup dispatched → `in_flight` decremented when done |
| executor pod restarts | SIGTERM → drain → wait for all jobs AND cleanups → exit 0 |

---

## What Each Layer Does

| Layer | Responsibility |
|---|---|
| Any invoker | Provide script files, read SSE stream, act on exit code |
| ginger-infra CLI | Read script files, POST to executor, stream SSE to stdout, propagate exit code |
| executor service | Job dispatch, stream routing, cleanup orchestration, drain management |
| WAMP broker | Authenticated delivery, NAT traversal |
| Presence service | Device discovery by capability |
| ginger-infra agent | Script execution, stdout/stderr publishing, cleanup, process cancellation |
| Device OS | The actual execution environment — bare metal, no containerisation |

Nothing fighting anything else. Each layer does exactly one thing.



# one-time, against whichever cluster KUBECONFIG points at:
ginger-infra install-tekton-crd \
  --image gingersociety/remote-task-controller:latest \
  --executor-url http://tekton-executor.infra.svc.cluster.local:8099/run-job

# verify:
kubectl get crd remotetasks.gingersociety.org
kubectl -n tekton-pipelines get deployment remote-task-controller
kubectl -n tekton-pipelines logs deploy/remote-task-controller -f

# then apply a RemoteTask and watch it:
kubectl apply -f my-remote-task.yaml
kubectl get remotetasks -A -w


kubectl patch deployment remote-task-controller -n tekton-pipelines --type='json' -p='[
  {
    "op": "add",
    "path": "/spec/template/spec/securityContext",
    "value": {
      "runAsNonRoot": true,
      "seccompProfile": { "type": "RuntimeDefault" }
    }
  },
  {
    "op": "add",
    "path": "/spec/template/spec/containers/0/securityContext",
    "value": {
      "allowPrivilegeEscalation": false,
      "capabilities": { "drop": ["ALL"] }
    }
  }
]'