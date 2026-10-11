// The bash tool's sandbox, using bubblewrap (bwrap): the same Linux kernel
// features containers use (namespaces), without a container runtime.
//
// Inside it:
//   - the whole system is visible but read-only;
//   - the home folder is replaced by an empty one, so secrets there (~/.ssh,
//     ~/.marv with the API key, browser profiles, …) can't be read, except a
//     few toolchain folders (bun, cargo, git config…), mounted read-only, with
//     the credential files inside them hidden (tokens inside whole config
//     files, like ~/.gitconfig or ~/.cargo/config.toml, can't be);
//   - the project folder is the only writable place (plus a private /tmp); a
//     caller may add folders that are visible read-only, e.g. a worktree's .git;
//   - bun's download cache is a throwaway one in that private /tmp (RAM-backed),
//     gone when the command ends: a shared or host cache would let one sandboxed
//     command (e.g. an install's lifecycle scripts) plant packages for another
//     context;
//   - there's no network unless the command asked for it;
//   - the environment starts empty, so API keys can't leak into commands.
// Approval decides *whether* a command runs; the sandbox limits *what an
// approved command can touch*.
import { existsSync, lstatSync, realpathSync } from "node:fs";
import { isAbsolute, join, relative } from "node:path";

/** Folders from the hidden home folder a command may need, mounted read-only. */
export const TOOLCHAINS = [
  ".bun",
  ".cargo",
  ".rustup",
  ".local/bin",
  ".local/share/pnpm",
  ".local/share/mise",
  ".asdf",
  ".nvm",
  ".volta",
  ".deno",
  ".pyenv",
  ".gitconfig",
  ".config/git",
  // Personal skills, so their scripts can run (the rest of ~/.marv, with the API key, stays hidden).
  ".marv/skills",
];

/**
 * PATH folders under the hidden home folder that no toolchain mount shows. A tool on PATH whose folder the sandbox
 * hides (`env: 'node': No such file or directory`, exit 127) is otherwise a silent mystery — and silently skips
 * Marv's own post-edit typecheck. Not the mounted ones, not inside the project, not folders that don't exist
 * (nothing to miss). Sorted by path, so a notice lists them in a stable order.
 */
export function hiddenPathFolders(path: string, home: string, root?: string, exists: (path: string) => boolean = existsSync): string[] {
  const found = new Set<string>();
  for (const dir of path.split(":")) {
    if (!isAbsolute(dir) || !contains(home, dir) || !exists(dir)) continue;
    // The project is mounted (writable): its node_modules/.bin or bin/ aren't hidden.
    if (root && contains(root, dir)) continue;
    if (TOOLCHAINS.some((toolchain) => contains(join(home, toolchain), dir))) continue;
    found.add(dir);
  }
  return [...found].sort();
}

interface SandboxOptions {
  root: string;
  home: string;
  network: boolean;
  /** PATH inside the sandbox. */
  path: string;
  /**
   * Extra folders the command may read but not change, e.g. a worktree's shared .git (so git status/diff/log
   * work), or the worktree around a subagent that works in one of its subfolders (mounted under the project,
   * which stays writable).
   */
  readOnly?: string[];
  /**
   * Paths inside the project that don't exist yet and must stay that way: each gets an empty read-only folder
   * mounted on it (a project without .git, so a command can't create a repository whose config git would run
   * outside the sandbox). bwrap leaves the empty mount point behind; the caller removes it.
   */
  placeholders?: string[];
  /** The project read-only too: for Marv's own checks, which run the project's code without asking (src/diagnostics). */
  projectReadOnly?: boolean;
  /** The file system, injectable for tests: whether a path exists (following symlinks), */
  exists?: (path: string) => boolean;
  /** what the path itself is (not following a final symlink), */
  stat?: (path: string) => "file" | "symlink" | null;
  /** and where it really leads (throws if it leads nowhere). */
  realpath?: (path: string) => string;
}

function lstatKind(path: string): "file" | "symlink" | null {
  try {
    const stats = lstatSync(path);
    return stats.isSymbolicLink() ? "symlink" : stats.isFile() ? "file" : null;
  } catch {
    return null;
  }
}

/**
 * The real path of an extra folder, after checking that mounting it is safe, and whether it holds the project
 * (it's then mounted before the project, so the project stays writable on top). bwrap follows symlinks, so the
 * check must too.
 */
function resolveExtra(dir: string, home: string, root: string): { real: string; around: boolean } {
  if (!isAbsolute(dir)) throw new Error(`Extra sandbox folder ${dir} must be an absolute path.`);
  let real: string;
  try {
    real = realpathSync(dir);
  } catch {
    throw new Error(`Extra sandbox folder ${dir} doesn't exist.`);
  }
  const realHome = realOrSelf(home);
  const realRoot = realOrSelf(root);
  // Mounting the home folder or a parent of it (even read-only) would expose the hidden home, with the API key in it.
  if (real === "/" || contains(real, realHome)) {
    throw new Error(`Refusing to make ${dir} visible in the sandbox: it is, or leads to, the home folder or one of its parents.`);
  }
  // Read-only over the project itself would leave nothing writable.
  if (real === realRoot) {
    throw new Error(`Refusing to make ${dir} read-only in the sandbox: it is the project folder.`);
  }
  return { real, around: contains(real, realRoot) };
}

/** Whether `outer` is `inner` or a parent of it. */
function contains(outer: string, inner: string): boolean {
  const rel = relative(outer, inner);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

function realOrSelf(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

/**
 * Credential files inside the toolchain folders above, replaced by an empty file: an approved (or
 * auto-approved) command could otherwise print them into the conversation, which goes to the model
 * provider. Tokens written into whole config files (~/.gitconfig, ~/.config/git/config,
 * ~/.cargo/config.toml) can't be hidden this way: the sandbox needs those files whole.
 */
const CREDENTIALS = [".cargo/credentials", ".cargo/credentials.toml", ".config/git/credentials"];

/**
 * Where to put /dev/null so the sandbox can't read a credential file, as paths inside the sandbox.
 * A plain file is masked where it is. bwrap can't mount on a symlink (and fails before the command
 * starts), so for a symlink (stow, chezmoi) the file it leads to is masked instead, wherever a
 * toolchain mount makes it visible; a link leading anywhere else already reads as missing, because
 * the home is hidden. Anything else is left alone.
 */
function credentialMasks(file: string, mounts: string[], stat: SandboxOptions["stat"] & {}, realpath: SandboxOptions["realpath"] & {}): string[] {
  const kind = stat(file);
  if (kind === "file") return [file];
  if (kind !== "symlink") return [];
  let target: string;
  try {
    target = realpath(file);
  } catch {
    return []; // dangling
  }
  if (stat(target) !== "file") return [];
  const masks: string[] = [];
  for (const mount of mounts) {
    let source: string;
    try {
      source = realpath(mount); // what bwrap mounted there
    } catch {
      continue;
    }
    // The real path has no symlinks below the mount point, so this is never a symlink inside the sandbox.
    if (contains(source, target)) masks.push(join(mount, relative(source, target)));
  }
  return masks;
}

/**
 * What the sandbox keeps of /run. It holds the host's sockets: the user's D-Bus (`systemd-run --user` starts any
 * program outside the sandbox), the GPG and SSH agents, the keyring, docker's, and, only offline, the display (with
 * network the sandbox shares the host's network namespace, whose abstract sockets include X11's). A read-only mount
 * doesn't stop a process from connecting to a socket, so /run becomes an empty folder, and only what commands need
 * from it is shown again, read-only: folders on PATH (NixOS keeps its programs in /run/current-system, fnm its node in
 * /run/user/<uid>/fnm_multishells) and, with network, the DNS config /etc/resolv.conf leads to (systemd-resolved
 * keeps it in /run). Never /run, /run/user or a user's whole folder, even if PATH names one: that's the sockets again.
 */
function runMounts(path: string, network: boolean, exists: (path: string) => boolean, realpath: (path: string) => string): string[] {
  const keep = path.split(":").filter((dir) => dir.startsWith("/run/") && !/^\/run(\/user(\/[^/]+)?)?\/?$/.test(dir) && exists(dir));
  if (network) {
    try {
      const dns = realpath("/etc/resolv.conf");
      if (dns.startsWith("/run/")) keep.push(dns);
    } catch {
      // no resolv.conf: nothing to show
    }
  }
  return ["--tmpfs", "/run", ...[...new Set(keep)].flatMap((p) => ["--ro-bind", p, p])];
}

/** bwrap's arguments (everything before `-- command`). Order matters: later mounts sit on top of earlier ones. */
export function sandboxArgs({
  root,
  home,
  network,
  path,
  readOnly = [],
  placeholders = [],
  projectReadOnly = false,
  exists = existsSync,
  stat = lstatKind,
  realpath = realpathSync,
}: SandboxOptions): string[] {
  const args = ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--tmpfs", home];
  args.push(...runMounts(path, network, exists, realpath));
  const mounts = TOOLCHAINS.map((dir) => join(home, dir)).filter((full) => exists(full));
  for (const full of mounts) args.push("--ro-bind", full, full);
  // Bind the resolved path (what was checked), at the path the caller gave. A folder around the project goes
  // first: mounted after it, it would cover the project and silently turn it read-only.
  const extras = readOnly.map((dir) => ({ dir, ...resolveExtra(dir, home, root) }));
  for (const { dir, real } of extras.filter((e) => e.around)) args.push("--ro-bind", real, dir);
  args.push(projectReadOnly ? "--ro-bind" : "--bind", root, root);
  for (const { dir, real } of extras.filter((e) => !e.around)) args.push("--ro-bind", real, dir);
  // A read-only project can't get the empty mount point bwrap creates, and nothing in it can create a .git anyway.
  if (!projectReadOnly) for (const path of placeholders) args.push("--tmpfs", path, "--remount-ro", path);
  // Last, on top of every other mount, so the empty file hides the real one and no later mount shows it again.
  const masks = new Set(CREDENTIALS.flatMap((file) => credentialMasks(join(home, file), mounts, stat, realpath)));
  for (const mask of masks) args.push("--ro-bind", "/dev/null", mask);
  if (!network) args.push("--unshare-net");
  args.push(
    "--unshare-pid", // its processes can't see or signal ours, and all die with it
    "--die-with-parent",
    "--new-session", // can't inject keystrokes into our terminal
    "--clearenv",
    "--setenv", "PATH", path,
    "--setenv", "HOME", home,
    "--setenv", "LANG", process.env.LANG ?? "C.UTF-8",
    "--setenv", "TERM", "dumb",
    "--setenv", "TMPDIR", "/tmp",
    // `bun install` needs a writable cache (it stages downloads inside it and fails at once if it can't).
    // Not the user's ~/.bun/install/cache, nor any cache that outlives the command: an install runs the
    // project's scripts, which could plant packages there for the user's own projects or another
    // sandbox. Packages land in the project's node_modules, which stays. (The private /tmp is a tmpfs,
    // so the cache takes RAM while the command runs.)
    "--setenv", "BUN_INSTALL_CACHE_DIR", "/tmp/bun-cache",
    "--chdir", root,
  );
  return args;
}

let available: boolean | undefined;

/** Whether bwrap is installed and allowed to create namespaces here (checked once). */
export function sandboxAvailable(): boolean {
  if (available === undefined) {
    try {
      available = Bun.spawnSync(["bwrap", "--ro-bind", "/", "/", "--unshare-net", "--unshare-pid", "--", "true"], { stdout: "ignore", stderr: "ignore" }).exitCode === 0;
    } catch {
      available = false;
    }
  }
  return available;
}
