import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, realpathSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { bash, formatOutput, looksLikeNetworkFailure, runCommand, touchesHiddenHome } from "../src/tools/bash.ts";
import { sandboxArgs, sandboxAvailable } from "../src/sandbox.ts";

describe("sandboxArgs", () => {
  const base = { root: "/home/me/proj", home: "/home/me", path: "/usr/bin", exists: (p: string) => p.endsWith(".bun") };

  describe("extra read-only folders", () => {
    let tmp: string;
    let home: string;
    let root: string;
    let git: string;
    beforeEach(() => {
      tmp = mkdtempSync(join(tmpdir(), "marv-extra-"));
      home = join(tmp, "home");
      root = join(home, "proj");
      git = join(tmp, "repo", ".git");
      mkdirSync(root, { recursive: true });
      mkdirSync(git, { recursive: true });
    });
    afterEach(() => rmSync(tmp, { recursive: true, force: true }));
    const args = (readOnly: string[]) => sandboxArgs({ root, home, path: "/usr/bin", network: false, readOnly });

    test("(a worktree's shared .git) are mounted after the home is hidden and the root bound", () => {
      const a = args([git]);
      expect(a.join(" ")).toContain(`--ro-bind ${git} ${git}`);
      const at = a.lastIndexOf(git);
      expect(at).toBeGreaterThan(a.indexOf(home)); // after the home tmpfs
      expect(at).toBeGreaterThan(a.indexOf(root)); // after the root bind
    });

    test("never add a read-write bind", () => {
      const a = args([git]);
      // Only the project is writable.
      const writable = a.flatMap((x, i) => (x === "--bind" ? [a[i + 1]] : []));
      expect(writable).toEqual([root]);
    });

    test("that would expose the home folder or the whole system are refused", () => {
      for (const dir of ["/", tmp, home, "relative"]) expect(() => args([dir])).toThrow();
    });

    test("that are symlinks to the home folder are refused, and others are bound by their real path", () => {
      const link = join(tmp, "link");
      symlinkSync(home, link);
      expect(() => args([link])).toThrow();
      const alias = join(tmp, "alias");
      symlinkSync(git, alias);
      const a = args([alias]);
      const real = realpathSync(git);
      expect(a.join(" ")).toContain(`--ro-bind ${real} ${alias}`);
    });

    test("around the project (a worktree whose subfolder is the project) go before it, so the project stays writable", () => {
      const tree = join(tmp, "tree");
      const pkg = join(tree, "pkg");
      mkdirSync(pkg, { recursive: true });
      const a = sandboxArgs({ root: pkg, home, path: "/usr/bin", network: false, readOnly: [tree, git] });
      const line = a.join(" ");
      expect(line).toContain(`--ro-bind ${realpathSync(tree)} ${tree} --bind ${pkg} ${pkg} --ro-bind ${realpathSync(git)} ${git}`);
    });

    test("that equal the project, or hold the home folder, are refused", () => {
      expect(() => args([root])).toThrow();
      expect(() => args([home])).toThrow();
      const inner = join(root, "sub");
      mkdirSync(inner);
      expect(args([inner]).join(" ")).toContain(`--ro-bind ${realpathSync(inner)} ${inner}`); // inside the project is fine
    });

    test("that are missing fail and name the folder", () => {
      expect(() => args([join(tmp, "nope")])).toThrow(/nope/);
    });
  });

  test("/run is an empty folder (it holds the host's sockets), with the PATH folders and the DNS config that live in it shown again, read-only", () => {
    const realpath = (p: string) => (p === "/etc/resolv.conf" ? "/run/systemd/resolve/stub-resolv.conf" : p);
    const exists = (p: string) => p.startsWith("/run/") || p.endsWith(".bun");
    const path = "/run/current-system/sw/bin:/usr/bin:/run/user/1000/fnm_multishells/42/bin";
    const offline = sandboxArgs({ ...base, path, network: false, exists, realpath }).join(" ");
    expect(offline).toContain("--tmpfs /run");
    expect(offline).toContain("--ro-bind /run/current-system/sw/bin /run/current-system/sw/bin");
    expect(offline).toContain("--ro-bind /run/user/1000/fnm_multishells/42/bin /run/user/1000/fnm_multishells/42/bin");
    expect(offline).not.toContain("resolv"); // no network, no DNS
    const online = sandboxArgs({ ...base, path, network: true, exists, realpath }).join(" ");
    expect(online).toContain("--ro-bind /run/systemd/resolve/stub-resolv.conf /run/systemd/resolve/stub-resolv.conf");
    // The empty /run comes first: mounted after them, it would hide what it's meant to let through.
    expect(online.indexOf("--tmpfs /run")).toBeLessThan(online.indexOf("--ro-bind /run/systemd"));
  });

  test("read-only system, hidden home, writable project, no network, clean env", () => {
    const args = sandboxArgs({ ...base, network: false }).join(" ");
    expect(args).toContain("--ro-bind / /");
    expect(args).toContain("--tmpfs /home/me"); // home hidden…
    expect(args).toContain("--ro-bind /home/me/.bun /home/me/.bun"); // …except toolchains, read-only
    expect(args).not.toContain(".cargo"); // only the ones that exist
    expect(args).toContain("--bind /home/me/proj /home/me/proj");
    expect(args).toContain("--unshare-net");
    expect(args).toContain("--clearenv");
    expect(args).toContain("--chdir /home/me/proj");
  });

  test("projectReadOnly mounts the project read-only (Marv's own checks run project code unasked)", () => {
    const args = sandboxArgs({ ...base, network: false, projectReadOnly: true }).join(" ");
    expect(args).toContain("--ro-bind /home/me/proj /home/me/proj");
    expect(args).not.toContain("--bind /home/me/proj /home/me/proj");
  });

  test("projectReadOnly skips placeholders (bwrap can't create them in a read-only project)", () => {
    const args = sandboxArgs({ ...base, network: false, projectReadOnly: true, placeholders: ["/home/me/proj/.git"] }).join(" ");
    expect(args).not.toContain("--tmpfs /home/me/proj/.git");
  });

  test("personal skills are readable, the rest of ~/.marv is not", () => {
    const args = sandboxArgs({ ...base, network: false, exists: (p) => p.endsWith(".marv/skills") }).join(" ");
    expect(args).toContain("--ro-bind /home/me/.marv/skills /home/me/.marv/skills");
    expect(args).not.toContain("/home/me/.marv /home/me/.marv");
  });

  test("network can be allowed per command", () => {
    expect(sandboxArgs({ ...base, network: true })).not.toContain("--unshare-net");
  });

  test("bun's cache is a throwaway one in the private /tmp, never the user's (network or not)", () => {
    const exists = (p: string) => p.endsWith(".bun") || p.endsWith(".bun/install/cache");
    for (const network of [false, true]) {
      const args = sandboxArgs({ ...base, exists, network }).join(" ");
      expect(args).toContain("--setenv BUN_INSTALL_CACHE_DIR /tmp/bun-cache");
      // A shared or host cache would let one sandboxed command plant packages for another context.
      expect(args).not.toContain("--bind /home/me/.bun/install/cache");
      expect(args).not.toContain("sandbox-cache");
    }
  });

  test("credential files inside the read-only toolchain folders are hidden", () => {
    const secrets = ["/home/me/.cargo/credentials", "/home/me/.cargo/credentials.toml", "/home/me/.config/git/credentials"];
    const exists = (p: string) => [".cargo", ".config/git"].some((dir) => p.endsWith(dir)) || secrets.includes(p);
    const stat = (p: string) => (secrets.includes(p) ? ("file" as const) : null);
    const args = sandboxArgs({ ...base, exists, stat, network: false });
    const joined = args.join(" ");
    for (const secret of secrets) {
      expect(joined).toContain(`--ro-bind /dev/null ${secret}`);
      // After the toolchain mounts, so /dev/null sits on top of the real file.
      expect(args.lastIndexOf(secret)).toBeGreaterThan(Math.max(args.lastIndexOf("/home/me/.cargo"), args.lastIndexOf("/home/me/.config/git")));
      // After every other mount too (project, extra read-only folders), so no later mount can show the file again.
      expect(args.lastIndexOf(secret)).toBeGreaterThan(args.findIndex((arg, i) => arg === "--bind" && args[i + 1] === base.root));
    }
    // Ones that don't exist aren't mounted (bwrap would have to create them).
    expect(sandboxArgs({ ...base, exists: (p) => p.endsWith(".cargo"), stat: () => null, network: false }).join(" ")).not.toContain("/dev/null");
  });

  describe("symlinked credential files (stow, chezmoi)", () => {
    // bwrap refuses to mount on a symlink, so masking the link itself would stop every command from starting.
    const exists = (p: string) => p.endsWith(".cargo") || p.endsWith(".config/git");
    const sandbox = (files: Record<string, "file" | "symlink">, links: Record<string, string>) =>
      sandboxArgs({
        ...base,
        network: false,
        exists,
        stat: (p) => files[p] ?? null,
        realpath: (p) => links[p] ?? p,
      }).join(" ");

    test("pointing inside a mounted toolchain folder: the target is masked", () => {
      const args = sandbox(
        { "/home/me/.cargo/credentials": "symlink", "/home/me/.cargo/credentials.toml": "file" },
        { "/home/me/.cargo/credentials": "/home/me/.cargo/credentials.toml" },
      );
      expect(args).toContain("--ro-bind /dev/null /home/me/.cargo/credentials.toml");
      expect(args).not.toContain("/dev/null /home/me/.cargo/credentials ");
    });

    test("into a toolchain folder that is itself a symlink: masked where the sandbox mounts it", () => {
      const args = sandbox(
        { "/home/me/.config/git/credentials": "symlink", "/home/me/dotfiles/git/secret": "file" },
        { "/home/me/.config/git": "/home/me/dotfiles/git", "/home/me/.config/git/credentials": "/home/me/dotfiles/git/secret" },
      );
      expect(args).toContain("--ro-bind /dev/null /home/me/.config/git/secret");
      expect(args).not.toContain("/dev/null /home/me/.config/git/credentials");
    });

    test("pointing outside the mounted folders: skipped (the hidden home already makes it read as missing)", () => {
      const args = sandbox(
        { "/home/me/.cargo/credentials.toml": "symlink", "/home/me/dotfiles/cargo/credentials.toml": "file" },
        { "/home/me/.cargo/credentials.toml": "/home/me/dotfiles/cargo/credentials.toml" },
      );
      expect(args).not.toContain("/dev/null");
    });

    test("dangling, or pointing at a folder: skipped", () => {
      expect(sandbox({ "/home/me/.cargo/credentials": "symlink" }, { "/home/me/.cargo/credentials": "/home/me/.cargo/registry" })).not.toContain("/dev/null");
      const dangling = sandboxArgs({ ...base, network: false, exists, stat: () => "symlink", realpath: () => { throw new Error("ENOENT"); } });
      expect(dangling.join(" ")).not.toContain("/dev/null");
    });
  });

  test("the project is mounted after the home folder is hidden (so it stays visible)", () => {
    const args = sandboxArgs({ ...base, network: false });
    expect(args.indexOf("/home/me/proj")).toBeGreaterThan(args.indexOf("--tmpfs", args.indexOf("/tmp") + 1));
  });
});

describe("runCommand", () => {
  let root: string;
  beforeEach(async () => {
    root = await mkdtemp(join(tmpdir(), "marv-bash-"));
  });
  afterEach(() => rm(root, { recursive: true, force: true }));

  const run = (command: string, extra: Partial<Parameters<typeof runCommand>[0]> = {}) =>
    runCommand({ command, root, sandbox: false, network: false, timeoutMs: 10_000, ...extra });

  test("returns combined output and the exit code", async () => {
    const result = await run("echo out; echo err >&2; exit 3");
    expect(result.output).toBe("out\nerr\n");
    expect(result.exitCode).toBe(3);
  });

  test("runs in the project folder", async () => {
    await writeFile(join(root, "hello.txt"), "hi");
    expect((await run("cat hello.txt")).output).toBe("hi");
  });

  test("kills commands that run too long", async () => {
    const result = await run("sleep 5; echo never", { timeoutMs: 200 });
    expect(result.timedOut).toBe(true);
    expect(result.output).not.toContain("never");
  });

  test("stops when aborted (ctrl+c)", async () => {
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 100);
    const started = Date.now();
    const result = await run("sleep 5", { signal: controller.signal });
    expect(Date.now() - started).toBeLessThan(2000);
    expect(result.aborted).toBe(true);
  });

  test("doesn't pass API keys through to the command", async () => {
    process.env.OPENROUTER_API_KEY = "sk-or-secret";
    try {
      expect((await run("echo key=$OPENROUTER_API_KEY")).output).toBe("key=\n");
    } finally {
      delete process.env.OPENROUTER_API_KEY;
    }
  });

  describe.if(sandboxAvailable())("inside the bubblewrap sandbox", () => {
    const sandboxed = (command: string, network = false) => run(command, { sandbox: true, network });

    test("can't reach the host's sockets under /run: the user's D-Bus (systemd-run starts programs outside), GPG and SSH agents, docker", async () => {
      // A folder on the host, outside the project: the sandbox can't write it, so a file there means a command ran outside.
      const outside = mkdtempSync(join(tmpdir(), "marv-escape-"));
      const marker = join(outside, "escaped");
      try {
        const result = await sandboxed(
          'echo "run:[$(ls -A /run)]"; test -e /var/run/docker.sock && echo DOCKER; ' +
            `XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --wait --collect --quiet /usr/bin/touch ${marker} 2>&1; echo done`,
        );
        expect(result.output).toContain("done");
        expect(result.output).not.toContain("DOCKER");
        expect(result.output).toContain("run:[]"); // nothing of the host's /run (this machine's PATH and DNS don't live there)
        expect(existsSync(marker)).toBe(false);
      } finally {
        rmSync(outside, { recursive: true, force: true });
      }
    });

    test("can write the project, but not the system", async () => {
      expect((await sandboxed("touch made-it && ls made-it")).output).toBe("made-it\n");
      const system = await sandboxed("touch /usr/marv-probe");
      expect(system.exitCode).not.toBe(0);
    });

    test("with projectReadOnly, the project can be read but not changed", async () => {
      await writeFile(join(root, "kept.txt"), "kept\n");
      const result = await runCommand({ command: "cat kept.txt; touch made-it; echo changed > kept.txt", root, sandbox: true, network: false, projectReadOnly: true, timeoutMs: 10_000 });
      expect(result.output).toContain("kept");
      expect(result.output).toContain("Read-only file system");
      expect(existsSync(join(root, "made-it"))).toBe(false);
      expect(await readFile(join(root, "kept.txt"), "utf8")).toBe("kept\n");
    });

    test("can't see the home folder (keys, ssh, Marv's config)", async () => {
      const result = await sandboxed(`ls -A ${homedir()}/.ssh ${homedir()}/.marv 2>&1; echo done`);
      expect(result.output).toContain("No such file");
      expect(result.output).not.toContain("config.json");
    });

    test("a symlinked credential file doesn't stop commands from starting, and stays hidden", async () => {
      // A fake home with a stow-style link pointing outside ~/.cargo, next to a plain credentials file.
      const home = join(root, "home");
      mkdirSync(join(home, ".cargo"), { recursive: true });
      mkdirSync(join(home, "dotfiles"));
      await writeFile(join(home, "dotfiles", "cargo-credentials"), "token = \"SECRET\"");
      await writeFile(join(home, ".cargo", "credentials.toml"), "token = \"SECRET\"");
      symlinkSync(join(home, "dotfiles", "cargo-credentials"), join(home, ".cargo", "credentials"));
      const project = join(root, "project");
      mkdirSync(project);
      const result = await run("cat ~/.cargo/credentials ~/.cargo/credentials.toml; echo started", { sandbox: true, root: project, home });
      expect(result.output).toContain("started");
      expect(result.output).not.toContain("SECRET");
    });

    test("when bwrap itself can't start the sandbox, its error is shown", async () => {
      // A read-only extra given as a symlink inside the project: bwrap won't mount on a symlink.
      const elsewhere = await mkdtemp(join(tmpdir(), "marv-elsewhere-"));
      try {
        symlinkSync(elsewhere, join(root, "link"));
        const result = await run("echo never", { sandbox: true, readOnly: [join(root, "link")] });
        expect(result.exitCode).not.toBe(0);
        expect(result.output).toStartWith("[sandbox failed to start: bwrap: ");
        expect(result.output).toContain("symlink");
      } finally {
        await rm(elsewhere, { recursive: true, force: true });
      }
    });

    test("has no network unless asked", async () => {
      const offline = await sandboxed("bun -e 'await fetch(\"http://1.1.1.1\", { signal: AbortSignal.timeout(2000) })' 2>&1; echo exit=$?");
      expect(offline.output).toMatch(/exit=[1-9]/);
    });
  });
});

describe("touchesHiddenHome", () => {
  const home = "/home/me";
  const root = "/home/me/proj";
  const touches = (command: string) => touchesHiddenHome(command, root, home);

  test("the home folder or anything hidden in it", () => {
    expect(touches("ls -la ~/.marv/")).toBe(true);
    expect(touches("find ~ -maxdepth 4 -name trajectories")).toBe(true);
    expect(touches("cd ~ && ls")).toBe(true);
    expect(touches('cat "$HOME/.ssh/config"')).toBe(true);
    expect(touches("ls ${HOME}")).toBe(true);
    expect(touches("ls /home/me/.config 2>/dev/null")).toBe(true);
    expect(touches("echo x; ls ~/.marv/config.json|head")).toBe(true);
  });

  test("not the project, the folders the sandbox shows, or a ~ that isn't the home folder", () => {
    expect(touches("ls -la ~/.bun/bin/ | grep marv")).toBe(false);
    expect(touches("ls ~/.marv/skills/notes")).toBe(false);
    expect(touches("cat ~/proj/src/a.ts")).toBe(false);
    expect(touches("cat /home/me/proj/package.json")).toBe(false);
    expect(touches("git log HEAD~1 && git diff main~2")).toBe(false);
    expect(touches("ls /home/meow ~other")).toBe(false);
    expect(touches("bun test")).toBe(false);
  });
});

describe("bash's note about the hidden home folder", () => {
  let root: string;
  beforeEach(async () => {
    root = await mkdtemp(join(tmpdir(), "marv-bash-"));
  });
  afterEach(() => rm(root, { recursive: true, force: true }));
  const run = (sandbox: boolean) => bash.run({ command: "echo ~/.marv" }, { root, sandbox });

  test.if(sandboxAvailable())("is added in the sandbox, after the output, without counting as output", async () => {
    const result = await run(true);
    expect(result.output).toContain("Marv: ");
    expect(result.output).toContain("stand-in");
    expect(result.output.indexOf("/.marv")).toBeLessThan(result.output.indexOf("Marv: "));
    expect(result.summary).toBe("exit 0 · 1 line of output");
  });

  test("isn't added without the sandbox, where the home folder is the real one", async () => {
    expect((await run(false)).output).not.toContain("Marv: ");
  });
});

describe("looksLikeNetworkFailure", () => {
  test("network errors in the output", () => {
    expect(looksLikeNetworkFailure("./fetch.sh", "curl: (6) Could not resolve host: example.com")).toBe(true);
    expect(looksLikeNetworkFailure("bun run x", "TypeError: getaddrinfo ETIMEOUT example.com")).toBe(true);
    expect(looksLikeNetworkFailure("bun run x", 'code: "ConnectionRefused"')).toBe(true);
    expect(looksLikeNetworkFailure("make", "dns error: Temporary failure in name resolution")).toBe(true);
    expect(looksLikeNetworkFailure("python3 a.py", "OSError: [Errno 101] Network is unreachable")).toBe(true);
    expect(looksLikeNetworkFailure("node a.js", "Error: connect ECONNREFUSED 1.2.3.4:443")).toBe(true);
  });

  test("commands that need the network, whatever they print", () => {
    expect(looksLikeNetworkFailure("gh auth status", "The token in default is invalid.")).toBe(true);
    expect(looksLikeNetworkFailure("cd app && git pull --rebase", "")).toBe(true);
    expect(looksLikeNetworkFailure("git clone https://github.com/a/b", "")).toBe(true);
    expect(looksLikeNetworkFailure("bun install", "error: lockfile had changes")).toBe(true);
    expect(looksLikeNetworkFailure("npm i zod", "")).toBe(true);
    expect(looksLikeNetworkFailure("ls; curl -sS https://x.dev", "")).toBe(true);
  });

  test("not ordinary failures", () => {
    expect(looksLikeNetworkFailure("bun test", "1 fail\nexpect(received).toBe(expected)")).toBe(false);
    expect(looksLikeNetworkFailure("git status", "fatal: not a git repository")).toBe(false);
    expect(looksLikeNetworkFailure("git commit -m x", "nothing to commit")).toBe(false);
    expect(looksLikeNetworkFailure("cat ghost.txt", "No such file or directory")).toBe(false);
    expect(looksLikeNetworkFailure("bun run build", "error: Could not resolve: './missing.ts'")).toBe(false);
    expect(looksLikeNetworkFailure("ls src/gh", "")).toBe(false);
    // npx and bunx usually run a local tool; only a network error in their output counts.
    expect(looksLikeNetworkFailure("npx tsc --noEmit", "src/a.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.")).toBe(false);
    expect(looksLikeNetworkFailure("bunx eslint .", "src/a.ts\n  3:7  error  'x' is defined but never used  no-unused-vars")).toBe(false);
    expect(looksLikeNetworkFailure("bunx cowsay hi", "error: getaddrinfo EAI_AGAIN registry.npmjs.org")).toBe(true);
  });
});

describe("bash's note about the blocked network", () => {
  let root: string;
  beforeEach(async () => {
    root = await mkdtemp(join(tmpdir(), "marv-bash-"));
  });
  afterEach(() => rm(root, { recursive: true, force: true }));
  const failing = "echo 'curl: (6) Could not resolve host: example.com'; exit 6";

  test.if(sandboxAvailable())("is added when a sandboxed command without network fails like that", async () => {
    const result = await bash.run({ command: failing }, { root, sandbox: true });
    expect(result.output).toContain("Marv: this command ran in the sandbox without network");
    expect(result.output).toContain("network: true");
    expect(result.output.indexOf("Could not resolve")).toBeLessThan(result.output.indexOf("Marv: "));
    expect(result.summary).toBe("exit 6 · 1 line of output");
  });

  test.if(sandboxAvailable())("is added for a tool that needs the network, like gh", async () => {
    const result = await bash.run({ command: "gh() { echo 'The token in default is invalid.'; return 1; }; gh auth status" }, { root, sandbox: true });
    expect(result.output).toContain("network: true");
  });

  test.if(sandboxAvailable())("isn't added when the command succeeded, had network, or wasn't found", async () => {
    expect((await bash.run({ command: "echo 'Could not resolve host: x'" }, { root, sandbox: true })).output).not.toContain("Marv: ");
    expect((await bash.run({ command: failing, network: true }, { root, sandbox: true })).output).not.toContain("Marv: ");
    expect((await bash.run({ command: "echo 'gh: command not found'; exit 127" }, { root, sandbox: true })).output).not.toContain("Marv: ");
  });

  test("isn't added without the sandbox, where the network isn't blocked", async () => {
    expect((await bash.run({ command: failing }, { root, sandbox: false })).output).not.toContain("Marv: ");
  });
});

describe("formatOutput", () => {
  test("shows the command's output and exit code", () => {
    expect(formatOutput({ output: "ok\n", exitCode: 0, timedOut: false, aborted: false })).toBe("ok\n\n[exit code 0]");
    expect(formatOutput({ output: "", exitCode: 1, timedOut: false, aborted: false })).toBe("(no output)\n\n[exit code 1]");
  });

  test("keeps the start and the end of very long output", () => {
    const output = Array.from({ length: 5000 }, (_, i) => `line ${i}`).join("\n");
    const text = formatOutput({ output, exitCode: 0, timedOut: false, aborted: false });
    expect(text.length).toBeLessThan(32_000);
    expect(text).toContain("line 0\n");
    expect(text).toContain("line 4999");
    expect(text).toMatch(/characters omitted/);
  });

  test("says when it timed out", () => {
    expect(formatOutput({ output: "partial", exitCode: null, timedOut: true, aborted: false, timeoutMs: 1000 })).toContain("[timed out after 1s]");
  });
});
