/**
 * Rockbox TypeScript SDK — thin client for the sandbox-as-a-service API.
 *
 * Covers one-shot execution, persistent sessions, Gymnasium-style RL episodes
 * with EnvPool-style batched stepping, and file operations on session/episode
 * volumes. Zero dependencies (uses global fetch, Node 18+ / Deno / Bun).
 *
 *   import { Rockbox } from "rockbox-sdk";
 *
 *   const rb = new Rockbox("http://localhost:4000", "rb_OiYvy..."); // API key from /api/admin (dev tokens work in dev/test)
 *
 *   const result = await rb.execute({ language: "python", files: { "main.py": "print(2+2)" } });
 *
 *   const ep = await rb.episode(envSource, { seed: 42 });
 *   const ticks = await ep.steps([0, 1, 3, 2]);   // one round trip for all four
 *   await ep.close();
 */

export interface ExecuteResult {
  output: string;
  errors?: string;
  status: string;
  exit_code: number;
  request_id: string;
  vm_id: string;
  exec_time_ms?: number;
}

export interface FileEntry {
  name: string;
  path: string;
  type: string;
  size: number;
  mtime: number;
}

export interface Tick {
  observation_b64?: string;
  reward?: number;
  terminated?: boolean;
  truncated?: boolean;
  [k: string]: unknown;
}

export interface EpisodeStart {
  episode_id: string;
  vm_id: string;
  initial: Record<string, unknown>;
  seed?: number;
}

export class RockboxError extends Error {
  readonly status: number;
  readonly body: unknown;

  constructor(status: number, body: unknown) {
    super(`rockbox api ${status}: ${typeof body === "string" ? body : JSON.stringify(body)}`);
    this.name = "RockboxError";
    this.status = status;
    this.body = body;
  }
}

interface ClientOptions {
  timeoutMs?: number;
  fetchImpl?: typeof fetch;
}

export class Rockbox {
  private readonly baseUrl: string;
  private readonly token: string;
  private readonly timeoutMs: number;
  private readonly fetchImpl: typeof fetch;

  constructor(url = "http://localhost:4000", token = "", opts: ClientOptions = {}) {
    this.baseUrl = url.replace(/\/+$/, "");
    this.token = token;
    this.timeoutMs = opts.timeoutMs ?? 60_000;
    this.fetchImpl = opts.fetchImpl ?? fetch.bind(globalThis);
  }

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await this.fetchImpl(this.baseUrl + path, {
      method,
      headers: {
        authorization: `Bearer ${this.token}`,
        "content-type": "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(this.timeoutMs),
    });
    const text = await res.text();
    let parsed: unknown = text;
    try {
      parsed = text === "" ? {} : JSON.parse(text);
    } catch {
      // keep raw text
    }
    if (!res.ok) throw new RockboxError(res.status, parsed);
    return parsed as T;
  }

  // ------------------------------------------------------------- execution

  async execute(opts: {
    language: string;
    files: Record<string, string>;
    entrypoint?: string;
    runtime?: string;
    wallMs?: number;
    memoryMb?: number;
    env?: Record<string, string>;
  }): Promise<ExecuteResult> {
    const entrypoint = opts.entrypoint ?? Object.keys(opts.files)[0];
    const settings: Record<string, unknown> = {
      language: opts.language,
      entrypoint,
      files: Object.entries(opts.files).map(([path, content]) => ({ path, content })),
      limits: {
        wall_ms: opts.wallMs ?? 5000,
        ...(opts.memoryMb ? { memory_mb: opts.memoryMb } : {}),
      },
    };
    if (opts.runtime) settings.runtime = opts.runtime;
    if (opts.env) settings.env = opts.env;
    return this.request("POST", "/api/execute", { settings });
  }

  async usage(): Promise<Record<string, unknown>> {
    return this.request("GET", "/api/usage");
  }

  // -------------------------------------------------------------- sessions

  async startSession(opts: {
    language: string;
    entrypoint?: string;
    files?: Record<string, string>;
    wallMs?: number;
    runtime?: string;
  } = { language: "python" }): Promise<Session> {
    const entrypoint = opts.entrypoint ?? "main.py";
    const fileMap = opts.files ?? { [entrypoint]: "x = 0" };
    const settings: Record<string, unknown> = {
      mode: "session",
      language: opts.language,
      entrypoint,
      files: Object.entries(fileMap).map(([path, content]) => ({ path, content })),
      limits: { wall_ms: opts.wallMs ?? 5000 },
    };
    if (opts.runtime) settings.runtime = opts.runtime;
    const resp = await this.request<{ session_id: string; vm_id: string }>(
      "POST", "/api/sessions", { settings },
    );
    return new Session(this, resp.session_id, resp.vm_id);
  }

  // ------------------------------------------------------------------ RL

  async startEpisode(
    envSource: string,
    opts: { seed?: number; entrypoint?: string; wallMs?: number } = {},
  ): Promise<Episode> {
    const entrypoint = opts.entrypoint ?? "env.py";
    const settings: Record<string, unknown> = {
      mode: "rl_step",
      language: "python",
      runtime: "python-base",
      entrypoint,
      files: [{ path: entrypoint, content: envSource }],
      limits: { wall_ms: opts.wallMs ?? 30000 },
    };
    if (opts.seed !== undefined) settings.determinism = { seed: opts.seed };
    const start = await this.request<EpisodeStart>("POST", "/api/rl/episodes", { settings });
    return new Episode(this, start);
  }

  episode(
    envSource: string,
    opts: { seed?: number; entrypoint?: string; wallMs?: number } = {},
  ): Promise<Episode> {
    return this.startEpisode(envSource, opts);
  }

  // -------------------------------------------------------- internal HTTP

  _request<T>(method: string, path: string, body?: unknown): Promise<T> {
    return this.request(method, path, body);
  }
}

/** A live RL episode. Create via `Rockbox.episode()`. */
export class Episode {
  private readonly client: Rockbox;
  public readonly start: EpisodeStart;

  constructor(client: Rockbox, start: EpisodeStart) {
    this.client = client;
    this.start = start;
  }

  get id(): string {
    return this.start.episode_id;
  }

  /** Initial reset observation as raw bytes (base64-decoded). */
  get initialObservation(): Uint8Array {
    const b64 = (this.start.initial?.["observation"] as string) ?? "";
    return bytesFromB64(b64);
  }

  async step(action: number | Uint8Array): Promise<Tick> {
    const frame = typeof action === "number" ? new Uint8Array([action]) : action;
    const resp = await this.client._request<{ ticks: Tick[] }>(
      "POST", `/api/rl/episodes/${this.id}/step`,
      { action: b64FromBytes(frame) },
    );
    return decodeTick(resp.ticks[0]);
  }

  /** Batched stepping: one HTTP round trip for all actions. */
  async steps(actions: Array<number | Uint8Array>): Promise<Tick[]> {
    const frames = actions.map((a) =>
      typeof a === "number" ? b64FromBytes(new Uint8Array([a])) : b64FromBytes(a),
    );
    const resp = await this.client._request<{ ticks: Tick[] }>(
      "POST", `/api/rl/episodes/${this.id}/steps`,
      { actions: frames },
    );
    return resp.ticks.map(decodeTick);
  }

  // ---------------------------------------------------------------- files

  listFiles(path = "/"): Promise<FileEntry[]> {
    return this.client._request("GET", `/api/rl/episodes/${this.id}/files?path=${encodeURIComponent(path)}`);
  }

  async readFile(path: string): Promise<Uint8Array> {
    const resp = await this.client._request<{ content_b64: string }>(
      "GET", `/api/rl/episodes/${this.id}/files/content?path=${encodeURIComponent(path)}`,
    );
    return bytesFromB64(resp.content_b64);
  }

  writeFile(path: string, content: Uint8Array | string): Promise<{ path: string; size: number }> {
    const b64 = typeof content === "string" ? b64FromBytes(new TextEncoder().encode(content)) : b64FromBytes(content);
    return this.client._request("PUT", `/api/rl/episodes/${this.id}/files`, { path, content: b64 });
  }

  removeFile(path: string): Promise<{ deleted: string }> {
    return this.client._request("DELETE", `/api/rl/episodes/${this.id}/files?path=${encodeURIComponent(path)}`);
  }

  async close(): Promise<void> {
    await this.client._request("DELETE", `/api/rl/episodes/${this.id}`);
  }
}

// ------------------------------------------------------------ sessions

/** Persistent stateful session over the REST cell API. */
export class Session {
  private readonly client: Rockbox;
  public readonly id: string;
  public readonly vmId: string;

  constructor(client: Rockbox, id: string, vmId: string) {
    this.client = client;
    this.id = id;
    this.vmId = vmId;
  }

  /** Run a cell; returns when the orchestrator has queued it on the worker. */
  async executeCell(code: string, files: Record<string, string> = {}): Promise<void> {
    await this.client._request("POST", `/api/sessions/${this.id}/execute`, {
      code,
      files: Object.entries(files).map(([path, content]) => ({ path, content })),
    });
  }

  listFiles(path = "/"): Promise<FileEntry[]> {
    return this.client._request("GET", `/api/sessions/${this.id}/files?path=${encodeURIComponent(path)}`);
  }

  async readFile(path: string): Promise<Uint8Array> {
    const resp = await this.client._request<{ content_b64: string }>(
      "GET", `/api/sessions/${this.id}/files/content?path=${encodeURIComponent(path)}`,
    );
    return bytesFromB64(resp.content_b64);
  }

  writeFile(path: string, content: Uint8Array | string): Promise<{ path: string; size: number }> {
    const bytes = typeof content === "string" ? new TextEncoder().encode(content) : content;
    return this.client._request("PUT", `/api/sessions/${this.id}/files`, {
      path,
      content: b64FromBytes(bytes),
    });
  }

  removeFile(path: string): Promise<{ deleted: string }> {
    return this.client._request("DELETE", `/api/sessions/${this.id}/files?path=${encodeURIComponent(path)}`);
  }

  async close(): Promise<void> {
    await this.client._request("DELETE", `/api/sessions/${this.id}`);
  }
}

// ------------------------------------------------------------ helpers

function b64FromBytes(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
  // btoa exists in browsers/Node>=16; fall back to Buffer for odd runtimes.
  if (typeof btoa === "function") return btoa(binary);
  return Buffer.from(bytes).toString("base64");
}

function bytesFromB64(b64: string): Uint8Array {
  if (typeof atob === "function") {
    const binary = atob(b64);
    const out = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
    return out;
  }
  return new Uint8Array(Buffer.from(b64, "base64"));
}

function decodeTick(t: Tick): Tick {
  const tick = { ...t };
  if (typeof t.observation_b64 === "string") {
    tick["observation_bytes"] = bytesFromB64(t.observation_b64);
  }
  return tick;
}
