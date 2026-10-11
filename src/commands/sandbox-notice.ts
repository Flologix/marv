// The wording for PATH folders the sandbox can't see (they're under the home folder, outside the toolchains it
// mounts): said once at startup as a system message, and again by /sandbox, so one place keeps the two in step.
// Shared with the notice that runs without a command context.

/** The startup system message: what the user can do about it (a folder Marv doesn't know yet), not just the facts. */
export function hiddenPathFoldersNotice(folders: string[]): string {
  const one = folders.length === 1;
  return (
    `Your PATH has ${folders.length} folder${one ? "" : "s"} the sandbox can't see: ${folders.join(", ")}. ` +
    `Commands in Marv can't run the tool${one ? "" : "s"} in it, and tsc and other node_modules scripts may not run. ` +
    `Tell Marv's sandbox about it (an issue on the repository) or add the tool to a folder it does show, like ~/.local/bin.`
  );
}
