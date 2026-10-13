import { afterEach, describe, expect, mock, test } from "bun:test";
import { useState } from "react";
import { cleanup, render } from "ink-testing-library";
import { matchCommands, PromptInput } from "../src/ui/PromptInput.tsx";

const ENTER = "\r";
const UP = "\x1b[A";
const DOWN = "\x1b[B";
const TAB = "\t";
const RIGHT = "\x1b[C";
const ESC = "\x1b";
const tick = (ms = 30) => Bun.sleep(ms);

const COMMANDS = [
  { name: "help", description: "Show available commands" },
  { name: "model", description: "Switch model" },
  { name: "setup", description: "Change provider" },
  { name: "think", description: "Toggle thinking" },
];

afterEach(cleanup);

/** PromptInput is controlled; this holds its value the way the App does. */
function Harness({ onSubmit, history = [] }: { onSubmit: (text: string) => void; history?: string[] }) {
  const [value, setValue] = useState("");
  // Like the App: submitting clears the input.
  const submit = (text: string) => {
    setValue("");
    onSubmit(text);
  };
  return <PromptInput value={value} onChange={setValue} onSubmit={submit} history={history} busy={false} commands={COMMANDS} />;
}

async function press(stdin: { write: (data: string) => void }, ...keys: string[]) {
  for (const key of keys) {
    stdin.write(key);
    await tick();
  }
}

describe("slash command menu", () => {
  test("typing / lists every command with its description", async () => {
    const { stdin, lastFrame } = render(<Harness onSubmit={() => {}} />);
    await press(stdin, "/");
    for (const { name, description } of COMMANDS) {
      expect(lastFrame()).toContain(`/${name}`);
      expect(lastFrame()).toContain(description);
    }
  });

  test("narrows as you type, and Enter runs the highlighted command", async () => {
    const onSubmit = mock();
    const { stdin, lastFrame } = render(<Harness onSubmit={onSubmit} />);
    await press(stdin, "/se");
    expect(lastFrame()).toContain("/setup");
    expect(lastFrame()).not.toContain("/help");
    await press(stdin, ENTER);
    expect(onSubmit).toHaveBeenCalledWith("/setup");
  });

  test("↑/↓ move through the menu instead of input history", async () => {
    const onSubmit = mock();
    const { stdin } = render(<Harness onSubmit={onSubmit} history={["old message"]} />);
    await press(stdin, "/", DOWN, DOWN, ENTER);
    expect(onSubmit).toHaveBeenCalledWith("/setup");

    await press(stdin, "/", UP, ENTER); // wraps around to the last one
    expect(onSubmit).toHaveBeenLastCalledWith("/think");
  });

  test("Tab completes the command so you can add arguments, with the cursor at the end", async () => {
    const onSubmit = mock();
    const { stdin, lastFrame } = render(<Harness onSubmit={onSubmit} />);
    await press(stdin, "/th", TAB);
    expect(lastFrame()).toContain("> /think");
    expect(lastFrame()).not.toContain("Toggle thinking"); // menu closes once there's a space
    await press(stdin, "on", ENTER);
    expect(onSubmit).toHaveBeenCalledWith("/think on");
  });

  test("→ completes the highlighted command too, so you can keep typing", async () => {
    const onSubmit = mock();
    const { stdin, lastFrame } = render(<Harness onSubmit={onSubmit} />);
    await press(stdin, "/", DOWN, RIGHT); // highlight /model, then complete it
    expect(lastFrame()).toContain("> /model");
    expect(lastFrame()).not.toContain("Switch model"); // the menu closed
    await press(stdin, "qwen3.5:9b", ENTER);
    expect(onSubmit).toHaveBeenCalledWith("/model qwen3.5:9b");
  });

  test("Esc hides the menu, and Enter then submits exactly what was typed", async () => {
    const onSubmit = mock();
    const { stdin, lastFrame } = render(<Harness onSubmit={onSubmit} />);
    await press(stdin, "/h", ESC);
    await tick(100); // a lone ESC is held briefly to tell it apart from escape sequences
    expect(lastFrame()).not.toContain("Show available commands");
    await press(stdin, ENTER);
    expect(onSubmit).toHaveBeenCalledWith("/h");
  });

  test("no menu for ordinary messages, and ↑ still recalls history with the cursor at the end", async () => {
    const onSubmit = mock();
    const { stdin, lastFrame } = render(<Harness onSubmit={onSubmit} history={["hello"]} />);
    await press(stdin, UP, "!", ENTER);
    expect(lastFrame()).not.toContain("/help");
    expect(onSubmit).toHaveBeenCalledWith("hello!");
  });
});

test("matchCommands puts prefix matches first, then other matches", () => {
  expect(matchCommands("/", COMMANDS).map((c) => c.name)).toEqual(["help", "model", "setup", "think"]);
  expect(matchCommands("/e", COMMANDS).map((c) => c.name)).toEqual(["help", "model", "setup"]);
  expect(matchCommands("/s", COMMANDS).map((c) => c.name)).toEqual(["setup"]);
  expect(matchCommands("/model x", COMMANDS)).toEqual([]); // has arguments: menu closed
  expect(matchCommands("hi", COMMANDS)).toEqual([]);
});

test("an argument completion (\"mcp login github\") is offered once its command and subcommand are typed", () => {
  const withArgs = [...COMMANDS, { name: "mcp", description: "" }, { name: "mcp login github", description: "" }, { name: "mcp login linear", description: "" }, { name: "mcp logout github", description: "" }];
  expect(matchCommands("/mcp login ", withArgs).map((c) => c.name)).toEqual(["mcp login github", "mcp login linear"]);
  expect(matchCommands("/mcp login l", withArgs).map((c) => c.name)).toEqual(["mcp login linear"]);
  expect(matchCommands("/mcp logout ", withArgs).map((c) => c.name)).toEqual(["mcp logout github"]);
  // Not before: "/mcp " + Enter must still run /mcp, and "/m" lists commands, not arguments.
  expect(matchCommands("/mcp ", withArgs)).toEqual([]);
  expect(matchCommands("/m", withArgs).map((c) => c.name)).toEqual(["model", "mcp"]);
  expect(matchCommands("/mcp login github", withArgs).map((c) => c.name)).toEqual(["mcp login github"]);
});

test("ctrl+letter combinations don't type the letter", async () => {
  const onSubmit = mock();
  const { stdin } = render(<Harness onSubmit={onSubmit} />);
  await press(stdin, "\x0f", "\x01", "x", ENTER); // ctrl+o, ctrl+a, x
  expect(onSubmit).toHaveBeenCalledWith("x");
});

test("ctrl+arrows aren't typed, and still move the cursor", async () => {
  const onSubmit = mock();
  const { stdin } = render(<Harness onSubmit={onSubmit} />);
  await press(stdin, "a", "b", "\x1b[1;5D", "x", "\x1b[1;5C", "y", ENTER); // ctrl+←, ctrl+→
  expect(onSubmit).toHaveBeenCalledWith("axby");
});
