import { expect, test } from "bun:test";
import { hostEnv } from "../src/tools/bash.ts";
import { ghEnv } from "../src/tools/github/github.ts";

// A command outside the sandbox (#52) and gh (#47) run with the user's environment; what they print reaches the
// model. Secrets in it stay behind.
const ENV = {
  PATH: "/usr/bin",
  HOME: "/home/u",
  XDG_RUNTIME_DIR: "/run/user/1000",
  DBUS_SESSION_BUS_ADDRESS: "unix:path=/run/user/1000/bus",
  SSH_AUTH_SOCK: "/run/user/1000/ssh",
  OPENROUTER_API_KEY: "sk-or",
  MARV_MODEL: "m",
  ANTHROPIC_API_KEY: "sk-ant",
  GITHUB_TOKEN: "ghp_x",
  GH_TOKEN: "ghp_y",
  AWS_SECRET_ACCESS_KEY: "aws",
  AWS_ACCESS_KEY_ID: "AKIA",
  NPM_TOKEN: "npm",
  PGPASSWORD: "pg",
  DB_PASSWORD: "db",
  STRIPE_KEY: "sk_live",
  CLIENT_SECRET: "cs",
  GOOGLE_APPLICATION_CREDENTIALS: "/k.json",
  DATABASE_URL: "postgres://user:hunter2@db:5432/app",
  REDIS_URL: "redis://localhost:6379",
  KEYBOARD_LAYOUT: "us",
};

test("hostEnv keeps what host tools need and drops secrets: provider keys, tokens, passwords, keys, credentials in URLs", () => {
  const env = hostEnv(ENV);
  expect(Object.keys(env).sort()).toEqual(["DBUS_SESSION_BUS_ADDRESS", "HOME", "KEYBOARD_LAYOUT", "PATH", "REDIS_URL", "SSH_AUTH_SOCK", "XDG_RUNTIME_DIR"]);
});

test("ghEnv gets gh's own tokens back (a user logged in through GH_TOKEN), and nothing else secret", () => {
  const env = ghEnv(ENV);
  expect(env.GH_TOKEN).toBe("ghp_y");
  expect(env.GITHUB_TOKEN).toBe("ghp_x");
  for (const name of ["ANTHROPIC_API_KEY", "AWS_SECRET_ACCESS_KEY", "NPM_TOKEN", "DATABASE_URL", "OPENROUTER_API_KEY"]) expect(env[name]).toBeUndefined();
});
