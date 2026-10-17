// The app gallery: MCP servers known to work with Marv, each with what it needs. Adding one goes through the same
// lookup as /mcp add (the package is pinned and checked); the credentials typed here fill its `${VAR}`s in
// ~/.marv/mcp.json (yours only).

export interface Field {
  /** The variable the server reads. */
  name: string;
  label: string;
  placeholder?: string;
  value?: string;
  secret?: boolean;
}

export interface App {
  id: string;
  name: string;
  /** One line: what Marv can do with it. */
  blurb: string;
  /** A letter or two for its tile. */
  mark: string;
  /** For the lookup: an npm package, `pypi:<package>`, a URL or a GitHub repo. */
  source: string;
  fields: Field[];
  /** Header values, each referring to a field: `{ Authorization: "Bearer ${GITHUB_TOKEN}" }`. */
  headers?: Record<string, string>;
  /** How to get the credentials, shown above the fields. */
  setup?: { text: string; url: string };
  group: "work" | "build";
}

export const APPS: App[] = [
  {
    id: "gmail",
    name: "Gmail",
    blurb: "Read, search, draft and send mail. Sending asks you first.",
    mark: "M",
    source: "pypi:mcparmory-google-gmail",
    group: "work",
    setup: {
      text: "Make a Google OAuth client: in Google Cloud, enable the Gmail API, then Credentials → Create → OAuth client ID → Desktop app.",
      url: "https://console.cloud.google.com/apis/credentials",
    },
    fields: [
      { name: "OAUTH2_CLIENT_ID", label: "Client ID", placeholder: "…apps.googleusercontent.com" },
      { name: "OAUTH2_CLIENT_SECRET", label: "Client secret", secret: true },
      { name: "OAUTH2_SCOPES", label: "Scopes", value: "https://www.googleapis.com/auth/gmail.modify" },
    ],
  },
  {
    id: "slack",
    name: "Slack",
    blurb: "Read channels and threads, post and reply.",
    mark: "#",
    source: "slack-workspace-mcp-server",
    group: "work",
    setup: {
      text: "Create a Slack app for your workspace, add bot scopes (channels:history, chat:write…), install it, and copy the Bot User OAuth Token.",
      url: "https://api.slack.com/apps",
    },
    fields: [{ name: "SLACK_BOT_TOKEN", label: "Bot token", placeholder: "xoxb-…", secret: true }],
  },
  {
    id: "github",
    name: "GitHub",
    blurb: "Issues, pull requests, code search, CI runs.",
    mark: "GH",
    source: "https://api.githubcopilot.com/mcp/",
    group: "build",
    headers: { Authorization: "Bearer ${GITHUB_TOKEN}" },
    setup: { text: "Create a fine-grained personal access token for the repositories Marv may use.", url: "https://github.com/settings/personal-access-tokens" },
    fields: [{ name: "GITHUB_TOKEN", label: "Access token", placeholder: "github_pat_…", secret: true }],
  },
  {
    id: "kicad",
    name: "KiCad",
    blurb: "Schematics, PCB layout, ERC and DRC, manufacturing exports.",
    mark: "Ki",
    source: "pypi:mcp-server-kicad",
    group: "build",
    fields: [],
  },
];
