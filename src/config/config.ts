import { existsSync } from "node:fs";
import { chmod, copyFile, mkdir, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { z } from "zod";
import { DEFAULT_MAX_STEPS } from "../agent.ts";

// Configuration comes from two layers, and the later one wins:
//   1. ~/.marv/config.json  (written by the setup screen)
//   2. environment variables (OPENROUTER_API_KEY, MARV_MODEL, OLLAMA_HOST)
// The file is what we save; the resolved Config is what the app runs with.

export const PROVIDERS = ["openrouter", "ollama"] as const;
export type ProviderId = (typeof PROVIDERS)[number];

interface Preset {
  label: string;
  /** Where the API lives. */
  baseUrl: string;
  /** Env var that overrides the saved key; absent when no key is needed. */
  keyEnv?: string;
  keyUrl?: string;
  /** Used until a model is picked; empty means "must pick one in setup". */
  defaultModel: string;
}

// OpenRouter is reached through the OpenAI-compatible adapter, Ollama through
// its native API (see src/provider/index.ts).
export const PRESETS: Record<ProviderId, Preset> = {
  openrouter: {
    label: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    keyEnv: "OPENROUTER_API_KEY",
    keyUrl: "openrouter.ai/settings/keys",
    defaultModel: "anthropic/claude-sonnet-5.5",
  },
  ollama: { label: "Ollama", baseUrl: "http://localhost:11434", defaultModel: "" },
};

const isProvider = (value: unknown): value is ProviderId => PROVIDERS.includes(value as ProviderId);

const FileConfigSchema = z.object({
  // A provider Marv no longer has (e.g. the old "echo") is dropped rather than
  // rejected, so setup opens instead of Marv refusing to start.
  provider: z.preprocess((v) => (isProvider(v) ? v : undefined), z.enum(PROVIDERS).optional()),
  model: z.string().min(1).optional(),
  /** The OpenRouter key (the only provider that needs one so far). */
  apiKey: z.string().min(1).optional(),
  /** Overrides the preset endpoint, e.g. Ollama on another machine. */
  baseUrl: z.string().url().optional(),
  /** Let thinking models reason before answering (slower, often better). */
  thinking: z.boolean().optional(),
  /** Ollama's context window in tokens (num_ctx). Bigger holds more code but needs more VRAM. */
  contextLength: z.number().int().min(2048).optional(),
  /** Run bash commands in the bubblewrap sandbox (default on). */
  sandbox: z.boolean().optional(),
  /** Run what the sandbox confines without asking: sandboxed commands, edits outside .git (default on). */
  yolo: z.boolean().optional(),
  /** Log every turn to ~/.marv/trajectories (default on). */
  trajectories: z.boolean().optional(),
  /** Typecheck after the model changes files, and tell it about new errors (default on). */
  diagnostics: z.boolean().optional(),
  /** Steps a turn runs before it asks whether to keep going; 0 (or "off") never asks. */
  maxSteps: z
    .union([z.number().int().min(0), z.literal("off")])
    .optional()
    .superRefine((v, ctx) => {
      if (v === "off") return;
      if (v === undefined) return;
      // 1 is useless (it would ask after every step), so the floor is 2.
      if (v === 1) ctx.addIssue({ code: "custom", message: "too small: the smallest interval is 2" });
    }),
});

/** 32k fits fully on a 12 GB GPU for 9-12B models and holds a fair amount of code. */
export const DEFAULT_CONTEXT_LENGTH = 32768;

/** Exactly what is stored on disk. */
export type FileConfig = z.infer<typeof FileConfigSchema>;

/** What the app actually runs with, after env overrides are applied. */
export interface Config {
  provider: ProviderId;
  model: string;
  baseUrl: string;
  apiKey?: string;
  apiKeySource?: "env" | "file";
  thinking: boolean;
  contextLength: number;
  sandbox: boolean;
  yolo: boolean;
  trajectories: boolean;
  diagnostics: boolean;
  /**
   * Steps a turn runs before it asks whether to keep going (with an approver; without one it's a hard stop).
   * 0 never asks — the guard is off, like Claude Code's and Codex's step limits — and 1 can't happen (it would ask
   * after every step). Defaults to the agent loop's DEFAULT_MAX_STEPS (25). Subagents' own hard limit (50) is not
   * touched by 0.
   */
  maxSteps: number;
}

export type Env = Record<string, string | undefined>;

export class ConfigError extends Error {}

export function defaultConfigDir(env: Env): string {
  return env.MARV_CONFIG_DIR ?? join(homedir(), ".marv");
}

/**
 * MARV_MAX_STEPS, like MARV_MODEL: an env var beats the saved file, so an orchestrator can start workers
 * that never stop without editing anyone's config. "off" (case-insensitive) is 0, "0" is 0; anything
 * unparsable or 1 is ignored (the file value, or the default, stands).
 */
function maxStepsFromEnv(value: string | undefined, fallback: number): number {
  const text = value?.trim();
  if (text === undefined || text === "") return fallback;
  if (/^off$/i.test(text)) return 0;
  const n = Number(text);
  if (!Number.isInteger(n) || n < 0 || n === 1) return fallback;
  return n;
}

/**
 * Marv used to be called Ekko and kept its config in ~/.ekko. The first time
 * Marv runs, it copies that config (provider, model, API key) to ~/.marv,
 * keeping it private. The old folder is left alone. Returns whether it copied.
 */
export async function migrateLegacyConfig(home = homedir()): Promise<boolean> {
  const from = join(home, ".ekko", "config.json");
  const to = join(home, ".marv", "config.json");
  if (existsSync(to) || !existsSync(from)) return false;
  await mkdir(dirname(to), { recursive: true, mode: 0o700 });
  await copyFile(from, to);
  await chmod(to, 0o600);
  return true;
}

export class ConfigStore {
  readonly path: string;

  constructor(dir: string) {
    this.path = join(dir, "config.json");
  }

  /** Returns null on first run (no file yet). Throws ConfigError if the file is broken. */
  async load(): Promise<FileConfig | null> {
    const file = Bun.file(this.path);
    if (!(await file.exists())) return null;

    let raw: unknown;
    try {
      raw = await file.json();
    } catch {
      throw new ConfigError(`${this.path} is not valid JSON.`);
    }
    const parsed = FileConfigSchema.safeParse(raw);
    if (!parsed.success) {
      throw new ConfigError(`Invalid config in ${this.path}:\n${z.prettifyError(parsed.error)}`);
    }
    return parsed.data;
  }

  async save(config: FileConfig): Promise<void> {
    // The file can hold an API key, so only the current user may read it.
    await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
    await writeFile(this.path, JSON.stringify(config, null, 2) + "\n", { mode: 0o600 });
    await chmod(this.path, 0o600); // `mode` above only applies when the file is first created
  }
}

export function resolveConfig(file: FileConfig | null, env: Env): Config {
  const provider = file?.provider ?? "openrouter";
  const preset = PRESETS[provider];
  const envKey = (preset.keyEnv && env[preset.keyEnv]?.trim()) || undefined;
  const apiKey = preset.keyEnv ? (envKey ?? file?.apiKey) : undefined;
  return {
    provider,
    model: env.MARV_MODEL?.trim() || file?.model || preset.defaultModel,
    baseUrl: file?.baseUrl ?? (provider === "ollama" ? ollamaUrl(env.OLLAMA_HOST) : preset.baseUrl),
    apiKey,
    apiKeySource: envKey ? "env" : apiKey ? "file" : undefined,
    thinking: file?.thinking ?? false,
    contextLength: file?.contextLength ?? DEFAULT_CONTEXT_LENGTH,
    sandbox: file?.sandbox ?? true,
    yolo: file?.yolo ?? true,
    trajectories: file?.trajectories ?? true,
    diagnostics: file?.diagnostics ?? true,
    maxSteps: maxStepsFromEnv(env.MARV_MAX_STEPS, file?.maxSteps === "off" ? 0 : (file?.maxSteps ?? DEFAULT_MAX_STEPS)),
  };
}

/** OLLAMA_HOST is Ollama's own setting ("127.0.0.1:11434" or a full URL); honor it. */
function ollamaUrl(host: string | undefined): string {
  if (!host?.trim()) return PRESETS.ollama.baseUrl;
  const url = /^https?:\/\//.test(host) ? host : `http://${host}`;
  return url.replace(/\/+$/, "");
}

/** First run, no provider chosen, a provider that needs a key we don't have, or no model picked yet. */
export function needsSetup(file: FileConfig | null, config: Config): boolean {
  if (file?.provider === undefined || !config.model) return true;
  return Boolean(PRESETS[config.provider].keyEnv) && !config.apiKey;
}

/** "sk-or-v1-abc…wxyz", enough to recognise a key without revealing it. */
export function maskKey(key: string): string {
  return key.length > 16 ? `${key.slice(0, 10)}…${key.slice(-4)}` : "****";
}
