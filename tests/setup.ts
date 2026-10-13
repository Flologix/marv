// Loaded before every test file (bunfig.toml). Tests never reach GitHub: the github tool's availability check (#47)
// runs `gh auth status`, which asks GitHub, wherever Marv starts the way a user does (marv -p, the TUI), so it's off
// for the whole run, child processes included. Tests of the tool put a fake gh on PATH and turn it on themselves.
process.env.MARV_GITHUB = "off";
