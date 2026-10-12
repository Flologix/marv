// An issue tracker as an MCP server, for the evals that measure deferred MCP tools (#14). Local, deterministic, no
// network: `bun evals/mcp/tracker.ts <data.json> <out.json>` reads the issues from data.json (in evals/mcp/data,
// outside the task's workspace, so the model can only learn them through the server) and appends every comment
// added to out.json, which the runner copies into the workspace for the check.
//
// 25 tools with realistic descriptions and parameters, of which a task needs one to three: their specs together
// cost well over the deferral threshold (tests/eval-mcp.test.ts), the way a real server's do (GitHub's has dozens).
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

interface Comment {
  id: number;
  issue: number;
  author: string;
  body: string;
}
interface Issue {
  number: number;
  title: string;
  state: "open" | "closed";
  author: string;
  assignee?: string;
  labels: string[];
  milestone?: string;
  body: string;
  comments: Comment[];
}
interface Data {
  project: { key: string; name: string; description: string };
  users: { login: string; name: string; role: string }[];
  labels: { name: string; description: string }[];
  milestones: { title: string; due: string; description: string }[];
  issues: (Omit<Issue, "comments"> & { comments?: { author: string; body: string }[] })[];
}

/** A comment the agent added: what the task's check reads. */
export interface AddedComment {
  issue: number;
  body: string;
}

const text = (value: unknown) => ({ content: [{ type: "text" as const, text: typeof value === "string" ? value : JSON.stringify(value, null, 2) }] });
const failure = (message: string) => ({ content: [{ type: "text" as const, text: message }], isError: true });
const READ = { readOnlyHint: true };
/** `fields` applied to an answer: each object (or the one) keeps only those keys. */
const pick = (value: unknown, keys: string[]): unknown =>
  Array.isArray(value) ? value.map((v) => pick(v, keys)) : value && typeof value === "object" ? Object.fromEntries(Object.entries(value).filter(([k]) => keys.includes(k))) : value;
const WRITE = { readOnlyHint: false, destructiveHint: false };

const issueNumber = z.number().int().positive().describe("The issue's number, as shown in the tracker (e.g. 12 for #12).");
const project = z
  .string()
  .optional()
  .describe("The project's key (e.g. UTIL), as list_projects shows it. Defaults to the only project this tracker holds; required when it holds several.");
// What real trackers' read tools take too: the fields to return, to keep large answers small.
const fields = z
  .array(z.string())
  .optional()
  .describe('Only these fields of each item (e.g. ["number", "title", "state"]). Default: all of them. Unknown names are ignored.');

export function makeTracker(dataFile: string, outFile: string): McpServer {
  const data = JSON.parse(readFileSync(dataFile, "utf8")) as Data;
  let nextComment = 1;
  const issues: Issue[] = data.issues.map((i) => ({ ...i, comments: (i.comments ?? []).map((c) => ({ id: nextComment++, issue: i.number, ...c })) }));
  const subscriptions = new Set<number>();
  const find = (n: number) => issues.find((i) => i.number === n);
  const summary = (i: Issue) => ({ number: i.number, title: i.title, state: i.state, labels: i.labels, assignee: i.assignee ?? null });
  const added = (): AddedComment[] => (existsSync(outFile) ? (JSON.parse(readFileSync(outFile, "utf8")) as AddedComment[]) : []);

  const server = new McpServer({ name: "tracker", version: "1.0.0" });
  const tool = (
    name: string,
    description: string,
    inputSchema: z.ZodRawShape,
    annotations: { readOnlyHint: boolean },
    run: (args: Record<string, unknown>) => ReturnType<typeof text> | ReturnType<typeof failure>,
  ) => {
    // Read tools also take `fields`, as real trackers' do: it adds to their specs what a real server's would.
    const read = annotations === READ;
    server.registerTool(name, { description, inputSchema: read ? { ...inputSchema, fields } : inputSchema, annotations }, async (args) => {
      const result = run(args as Record<string, unknown>);
      const only = (args as { fields?: string[] }).fields;
      // A JSON answer is picked here; a tool that answers in another format (export_issues' CSV) picks its own.
      if (!read || !only?.length || "isError" in result) return result;
      try {
        return text(pick(JSON.parse(result.content[0]!.text), only));
      } catch {
        return result;
      }
    });
  };

  // --- What the tasks use ---
  tool(
    "get_issue",
    "Get one issue by its number: title, state, author, assignee, labels, milestone, the full description and every comment. Use this to read what an issue asks for before working on it.",
    { number: issueNumber, project, include_comments: z.boolean().optional().describe("Include the issue's comments (default true).") },
    READ,
    ({ number, include_comments }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      return text(include_comments === false ? { ...issue, comments: undefined } : issue);
    },
  );
  tool(
    "list_issues",
    "List the project's issues, newest first, without their descriptions. Filter by state, label or assignee; use get_issue to read one in full.",
    {
      state: z.enum(["open", "closed", "all"]).optional().describe("Which issues to list (default open)."),
      label: z.string().optional().describe("Only issues with this label."),
      assignee: z.string().optional().describe("Only issues assigned to this login."),
      limit: z.number().int().positive().optional().describe("At most this many (default 30)."),
    },
    READ,
    ({ state = "open", label, assignee, limit = 30 }) =>
      text(
        issues
          .filter((i) => state === "all" || i.state === state)
          .filter((i) => !label || i.labels.includes(label as string))
          .filter((i) => !assignee || i.assignee === assignee)
          .sort((a, b) => b.number - a.number)
          .slice(0, limit as number)
          .map(summary),
      ),
  );
  // Marked read-only although it writes: the evals run with no approver, which refuses any other kind. It writes
  // nothing but this fixture's own output file.
  tool(
    "add_comment",
    "Add a comment to an issue, as the current user. Markdown is supported. Comments are visible to everyone who can see the issue and notify its subscribers.",
    {
      number: issueNumber,
      body: z.string().min(1).describe("The comment's text (Markdown)."),
      project,
      reply_to: z.number().int().optional().describe("The id of a comment this one answers, to thread it."),
    },
    READ,
    ({ number, body }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      const comment: Comment = { id: nextComment++, issue: issue.number, author: "you", body: body as string };
      issue.comments.push(comment);
      writeFileSync(outFile, JSON.stringify([...added(), { issue: issue.number, body: comment.body }], null, 2));
      return text({ ok: true, comment });
    },
  );

  // --- Distractors: plausible, never needed ---
  tool(
    "search_issues",
    "Search issues by words in their titles and descriptions. Returns matching issues without their descriptions, best match first. Supports open and closed issues.",
    {
      query: z.string().describe("Words to look for."),
      state: z.enum(["open", "closed", "all"]).optional().describe("Which issues to search (default all)."),
      label: z.string().optional().describe("Only issues with this label."),
      limit: z.number().int().positive().optional().describe("At most this many (default 10)."),
    },
    READ,
    ({ query, state = "all", limit = 10 }) => {
      const words = String(query).toLowerCase().split(/\W+/).filter(Boolean);
      return text(
        issues
          .filter((i) => state === "all" || i.state === state)
          .filter((i) => words.some((w) => `${i.title} ${i.body}`.toLowerCase().includes(w)))
          .slice(0, limit as number)
          .map(summary),
      );
    },
  );
  tool(
    "create_issue",
    "Create a new issue in the project. The current user becomes its author. Labels, an assignee and a milestone can be set at once.",
    {
      title: z.string().describe("A one-line summary."),
      body: z.string().optional().describe("The description (Markdown)."),
      labels: z.array(z.string()).optional().describe("Label names to add."),
      assignee: z.string().optional().describe("The login to assign it to."),
    },
    WRITE,
    ({ title, body = "", labels = [], assignee }) => {
      const issue: Issue = { number: Math.max(...issues.map((i) => i.number)) + 1, title: title as string, state: "open", author: "you", body: body as string, labels: labels as string[], assignee: assignee as string | undefined, comments: [] };
      issues.push(issue);
      return text(summary(issue));
    },
  );
  const setState = (state: "open" | "closed") => (args: Record<string, unknown>) => {
    const issue = find(args.number as number);
    if (!issue) return failure(`No issue #${args.number}.`);
    issue.state = state;
    return text(summary(issue));
  };
  tool(
    "close_issue",
    "Close an issue, optionally with a reason. Closing notifies the issue's subscribers. A closed issue can be reopened with reopen_issue.",
    { number: issueNumber, reason: z.enum(["completed", "not_planned", "duplicate"]).optional().describe("Why it's closed (default completed)."), project },
    WRITE,
    setState("closed"),
  );
  tool(
    "reopen_issue",
    "Reopen a closed issue. Its comments and labels are kept. Reopening notifies the issue's subscribers.",
    { number: issueNumber, project, comment: z.string().optional().describe("A comment to add while reopening.") },
    WRITE,
    setState("open"),
  );
  tool(
    "assign_issue",
    "Assign an issue to a user, replacing the current assignee. Pass no login to unassign it. The new assignee is notified.",
    { number: issueNumber, login: z.string().optional().describe("Who to assign it to; omit to unassign."), project },
    WRITE,
    ({ number, login }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      issue.assignee = login as string | undefined;
      return text(summary(issue));
    },
  );
  tool(
    "add_label",
    "Add a label to an issue. The label must exist in the project (see list_labels). Adding a label the issue already has does nothing.",
    { number: issueNumber, label: z.string().describe("The label's name."), project },
    WRITE,
    ({ number, label }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      if (!issue.labels.includes(label as string)) issue.labels.push(label as string);
      return text(summary(issue));
    },
  );
  tool(
    "remove_label",
    "Remove a label from an issue. Removing a label the issue doesn't have does nothing. The label itself stays in the project.",
    { number: issueNumber, label: z.string().describe("The label's name."), project },
    WRITE,
    ({ number, label }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      issue.labels = issue.labels.filter((l) => l !== label);
      return text(summary(issue));
    },
  );
  tool(
    "list_labels",
    "List the labels defined in the project, with their descriptions. Use their names with add_label, remove_label and the label filters.",
    { project, limit: z.number().int().positive().optional().describe("At most this many (default all).") },
    READ,
    () => text(data.labels),
  );
  tool(
    "get_user",
    "Get a user by login: their display name and role in the project. Use list_users to find a login.",
    { login: z.string().describe("The user's login."), project },
    READ,
    ({ login }) => {
      const user = data.users.find((u) => u.login === login);
      return user ? text(user) : failure(`No user "${login}".`);
    },
  );
  tool(
    "list_users",
    "List the project's members with their logins, display names and roles. Useful for assigning issues or mentioning people.",
    { project, role: z.string().optional().describe("Only members with this role (e.g. maintainer).") },
    READ,
    ({ role }) => text(data.users.filter((u) => !role || u.role === role)),
  );
  tool(
    "list_milestones",
    "List the project's milestones with their due dates and descriptions, soonest first. Issues can belong to one milestone.",
    { project, state: z.enum(["open", "closed", "all"]).optional().describe("Which milestones to list (default open).") },
    READ,
    () => text([...data.milestones].sort((a, b) => a.due.localeCompare(b.due))),
  );
  tool(
    "get_milestone",
    "Get one milestone by title: its due date, its description, and the issues in it with their states. Shows how far along it is.",
    { title: z.string().describe("The milestone's title."), project },
    READ,
    ({ title }) => {
      const milestone = data.milestones.find((m) => m.title === title);
      if (!milestone) return failure(`No milestone "${title}".`);
      return text({ ...milestone, issues: issues.filter((i) => i.milestone === title).map(summary) });
    },
  );
  tool(
    "link_pull_request",
    "Link a pull request to an issue, so the issue shows it and closes when the pull request is merged. Both must be in the same project.",
    { number: issueNumber, pull_request: z.number().int().positive().describe("The pull request's number."), closes: z.boolean().optional().describe("Close the issue when it's merged (default true)."), project },
    WRITE,
    ({ number, pull_request }) => (find(number as number) ? text({ ok: true, issue: number, pull_request }) : failure(`No issue #${number}.`)),
  );
  tool(
    "list_comments",
    "List an issue's comments, oldest first, with their ids and authors. get_issue includes them too; this one pages through long threads.",
    { number: issueNumber, since: z.number().int().optional().describe("Only comments after this comment id."), limit: z.number().int().positive().optional().describe("At most this many (default 50)."), project },
    READ,
    ({ number, since = 0, limit = 50 }) => {
      const issue = find(number as number);
      if (!issue) return failure(`No issue #${number}.`);
      return text(issue.comments.filter((c) => c.id > (since as number)).slice(0, limit as number));
    },
  );
  tool(
    "edit_comment",
    "Change the text of a comment you wrote. The comment shows that it was edited. Other people's comments can't be edited.",
    { comment_id: z.number().int().describe("The comment's id (from get_issue or list_comments)."), body: z.string().describe("The new text (Markdown)."), project },
    WRITE,
    ({ comment_id }) => failure(`Comment ${comment_id} isn't yours to edit.`),
  );
  tool(
    "delete_comment",
    "Delete a comment you wrote. This can't be undone. Other people's comments can't be deleted.",
    { comment_id: z.number().int().describe("The comment's id (from get_issue or list_comments)."), project },
    WRITE,
    ({ comment_id }) => failure(`Comment ${comment_id} isn't yours to delete.`),
  );
  tool(
    "get_project",
    "Get the project's details: its key, name, description, and how many issues are open and closed.",
    { project, include_stats: z.boolean().optional().describe("Include issue counts (default true).") },
    READ,
    () => text({ ...data.project, open: issues.filter((i) => i.state === "open").length, closed: issues.filter((i) => i.state === "closed").length }),
  );
  tool(
    "list_projects",
    "List the projects on this tracker that you can see, with their keys and names. Most trackers here hold one project.",
    { limit: z.number().int().positive().optional().describe("At most this many (default all).")},
    READ,
    () => text([{ key: data.project.key, name: data.project.name }]),
  );
  tool(
    "subscribe",
    "Subscribe to an issue: you'll be notified of its comments and changes. Authors and assignees are subscribed already.",
    { number: issueNumber, project },
    WRITE,
    ({ number }) => (subscriptions.add(number as number), text({ ok: true, subscribed: number })),
  );
  tool(
    "unsubscribe",
    "Stop being notified about an issue. You can subscribe again at any time.",
    { number: issueNumber, project },
    WRITE,
    ({ number }) => (subscriptions.delete(number as number), text({ ok: true, unsubscribed: number })),
  );
  tool(
    "get_activity",
    "Get the project's recent activity: issues opened, closed and commented on, newest first. Good for a summary of what happened lately.",
    { project, limit: z.number().int().positive().optional().describe("At most this many events (default 20)."), user: z.string().optional().describe("Only this login's activity.") },
    READ,
    ({ limit = 20 }) =>
      text(
        issues
          .flatMap((i) => [{ event: i.state === "closed" ? "closed" : "opened", issue: i.number, by: i.author }, ...i.comments.map((c) => ({ event: "commented", issue: i.number, by: c.author }))])
          .slice(0, limit as number),
      ),
  );
  tool(
    "export_issues",
    "Export the project's issues as CSV or JSON, descriptions included. Large projects produce large exports: prefer list_issues or search_issues to find something.",
    { format: z.enum(["csv", "json"]).describe("The export's format."), state: z.enum(["open", "closed", "all"]).optional().describe("Which issues (default all)."), project },
    READ,
    ({ format, fields: only }) => {
      if (format === "json") return text(issues.map(({ comments: _, ...i }) => i));
      const cell = { number: (i: Issue) => String(i.number), title: (i: Issue) => `"${i.title.replace(/"/g, '""')}"`, state: (i: Issue) => i.state };
      const wanted = (only as string[] | undefined)?.filter((f): f is keyof typeof cell => Object.hasOwn(cell, f));
      const columns = wanted?.length ? wanted : (Object.keys(cell) as (keyof typeof cell)[]);
      return text([columns.join(","), ...issues.map((i) => columns.map((c) => cell[c](i)).join(","))].join("\n"));
    },
  );
  return server;
}

if (import.meta.main) {
  const [dataFile, outFile] = process.argv.slice(2);
  if (!dataFile || !outFile) {
    console.error("usage: bun evals/mcp/tracker.ts <data.json> <out.json>");
    process.exit(2);
  }
  await makeTracker(dataFile, outFile).connect(new StdioServerTransport());
}
