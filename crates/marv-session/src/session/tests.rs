use super::*;
use marv_core::ToolSpec;
use marv_provider::EventStream;

struct Silent;
impl Provider for Silent {
    fn name(&self) -> &str {
        "silent"
    }
    fn context_length(&self) -> Option<u64> {
        Some(1000)
    }
    fn stream(&self, _history: &[ChatTurn], _options: marv_provider::StreamOptions) -> EventStream {
        Box::pin(futures::stream::empty())
    }
}

fn session(edit: impl FnOnce(&mut SessionInit)) -> Result<MarvSession, SessionError> {
    let mut init = SessionInit::new("/tmp/marv-session-test", ProviderChoice::Option(ProviderOption::Custom(Arc::new(Silent))));
    init.cwd = Some("~/p".into());
    edit(&mut init);
    MarvSession::new(init)
}

// #82 review: one unknown name used to make the seeding fail (nothing approved) while the prompt and
// /permissions still named every name. Now it's dropped with a problem and the rest are seeded and named.
#[test]
fn an_unknown_pre_approved_name_is_dropped_and_the_rest_seeded() {
    let s = session(|i| i.pre_approved = vec!["web".into(), "netwrk".into()]).unwrap();
    assert!(s.problems().iter().any(|p| p.contains("\"netwrk\"") && p.ends_with("ignored")), "{:?}", s.problems());
    assert!(s.inner.always.contains("web:example.com"), "web is seeded");
    assert!(!s.inner.always.contains("bash:net:curl x"));
    assert_eq!(s.permissions().pre_approved, ["web"]);
    let prompt = s.system_prompt();
    assert!(prompt.contains("(they run without asking): web.") && !prompt.contains("netwrk"), "{prompt}");
    assert_eq!(s.system_prompt(), prompt, "byte-stable");
}

#[test]
fn a_custom_provider_is_used_and_named() {
    let s = session(|_| {}).unwrap();
    assert_eq!(s.provider_name(), "silent");
    assert_eq!(s.context_length(), Some(1000));
    assert!(!s.busy());
    assert!(s.system_prompt().contains("Working directory: ~/p"));
}

/// A project without `.git` (or with an empty one) is told about the sandbox's empty stand-in; one with its own isn't.
#[test]
fn a_project_without_git_hears_about_the_placeholder() {
    let base = std::env::temp_dir().join(format!("marv-git-{}-{}", std::process::id(), marv_fs::random_hex()));
    let (bare, repo, mount) = (base.join("bare"), base.join("repo"), base.join("mount"));
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    // bwrap's leftover mount point while another command runs: an empty `.git` is still no repository.
    std::fs::create_dir_all(mount.join(".git")).unwrap();
    let prompt = |root: &std::path::Path| session(|i| i.root = root.to_path_buf()).unwrap().system_prompt();
    assert!(prompt(&bare).contains(crate::prompt::GIT_PLACEHOLDER));
    assert!(prompt(&mount).contains(crate::prompt::GIT_PLACEHOLDER));
    assert!(!prompt(&repo).contains(crate::prompt::GIT_PLACEHOLDER));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn yolo_where_it_isnt_allowed_fails_and_configure_changes_nothing() {
    let err = session(|i| i.no_yolo = Some("home".into())).err().unwrap();
    assert_eq!(err.to_string(), "home");
    let s = session(|i| {
        i.no_yolo = Some("home".into());
        i.yolo = false;
    })
    .unwrap();
    assert!(s.configure(Configure { yolo: Some(true), ..Configure::default() }).is_err());
    assert!(!s.state().yolo);
}

/// An unsandboxed run (`files_anywhere` with the sandbox off: `marv -p --dangerously-run-unsandboxed`) is told its
/// commands run on the machine; a sandboxed one, or the sandbox merely off (the TUI can turn it back on), keeps the
/// sandboxed facts. Turning the sandbox on mid-session doesn't change the prompt (the cache).
#[test]
fn an_unsandboxed_run_says_so_in_its_access_facts() {
    let s = session(|i| {
        i.sandbox = false;
        i.files_anywhere = true;
    })
    .unwrap();
    assert!(s.system_prompt().contains(crate::prompt::UNSANDBOXED_FACTS), "{}", s.system_prompt());
    // Its bash spec says the network is on (2026-11-06), fixed for the session.
    let bash = |s: &MarvSession| s.inner.specs.iter().find(|t| t.name == "bash").cloned().unwrap();
    assert_eq!(bash(&s).description, marv_tools::bash::UNSANDBOXED_DESCRIPTION);
    let before = s.build_system();
    s.configure(Configure { sandbox: Some(true), ..Default::default() }).unwrap();
    assert_eq!(s.build_system(), before);
    assert_eq!(bash(&s).description, marv_tools::bash::UNSANDBOXED_DESCRIPTION);
    for sandbox in [true, false] {
        let other = session(|i| i.sandbox = sandbox).unwrap();
        assert!(!other.system_prompt().contains(crate::prompt::UNSANDBOXED_FACTS));
        assert_ne!(bash(&other).description, marv_tools::bash::UNSANDBOXED_DESCRIPTION);
    }
}

/// #74: in a Herdr pane the session offers the herdr tool, after the caller's tools, and its situation says so;
/// elsewhere neither. Both fixed for the session (the cache).
#[test]
fn in_herdr_the_herdr_tool_is_offered_and_the_situation_says_so() {
    let dir = std::env::temp_dir().join(format!("marv-session-herdr-{}", std::process::id()));
    let setup = crate::herdr_door::fake::fake_setup(&dir, 4);
    let s = session(|i| {
        i.herdr = Some(setup.clone());
        i.host_commands = true;
        i.tools = vec![Arc::new(Named("mine"))];
    })
    .unwrap();
    let names: Vec<&str> = s.inner.specs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(&names[names.len() - 2..], ["mine", "herdr"]);
    assert!(s.system_prompt().contains(crate::prompt::HERDR_SITUATION));
    assert_eq!(s.system_prompt(), s.build_system());
    assert!(s.inner.specs.last().unwrap().description.contains("spawn"));
    // Without host_commands, run and spawn can't work: neither the prompt nor the tool offers them.
    let reads = session(|i| i.herdr = Some(setup.clone())).unwrap();
    assert!(reads.system_prompt().contains(crate::prompt::HERDR_SITUATION_READS));
    assert!(!reads.system_prompt().contains(crate::prompt::HERDR_SITUATION));
    let spec = reads.inner.specs.iter().find(|t| t.name == "herdr").unwrap();
    assert!(!spec.description.contains("spawn (") && spec.description.contains("isn't available here"), "{}", spec.description);
    let plain = session(|_| {}).unwrap();
    assert!(!plain.inner.specs.iter().any(|t| t.name == "herdr"));
    assert!(!plain.system_prompt().contains("Herdr pane"));
    assert!(
        session(|i| {
            i.herdr = Some(setup.clone());
            i.tools = vec![Arc::new(Named("herdr"))];
        })
        .is_err()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn system_prompt_can_be_replaced_or_appended() {
    let s = session(|i| i.system_prompt = Some(SystemPrompt::Replace("Be brief.".into()))).unwrap();
    assert_eq!(s.system_prompt(), "Be brief.");
    let s = session(|i| i.system_prompt = Some(SystemPrompt::Append("Extra.".into()))).unwrap();
    assert!(s.system_prompt().ends_with("\n\nExtra."));
}

struct Named(&'static str);
#[async_trait::async_trait]
impl marv_tools::Tool for Named {
    type Input = serde_json::Value;
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "x"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Read
    }
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Project
    }
    fn label(&self, _input: &Self::Input) -> String {
        String::new()
    }
    async fn run(
        &self,
        _input: &Self::Input,
        _ctx: &marv_tools::ToolContext,
    ) -> Result<marv_tools::ToolOutput, marv_tools::ToolError> {
        Ok(marv_tools::ToolOutput::ok("", ""))
    }
}

#[test]
fn custom_tool_names_must_be_their_own() {
    let tools = |names: &[&'static str]| names.iter().map(|n| Arc::new(Named(n)) as ToolRef).collect::<Vec<_>>();
    assert!(check_tool_names(&tools(&["deploy"]), true, false).is_ok());
    assert!(check_tool_names(&tools(&["bash"]), true, false).unwrap_err().contains("already built in"));
    assert!(check_tool_names(&tools(&["mcp_call"]), true, false).unwrap_err().contains("already built in"));
    assert!(check_tool_names(&tools(&["mcp__x__y"]), true, false).unwrap_err().contains("mcp__"));
    assert!(check_tool_names(&tools(&["a", "a"]), true, false).unwrap_err().contains("Two tools"));
    // plan is a free name where the session doesn't offer it (#15).
    assert!(check_tool_names(&tools(&["plan"]), true, false).unwrap_err().contains("\"plan\" is already built in"));
    assert!(check_tool_names(&tools(&["plan"]), false, false).is_ok());
    // So is github (#47).
    assert!(check_tool_names(&tools(&["github"]), true, false).is_ok());
    assert!(check_tool_names(&tools(&["github"]), true, true).unwrap_err().contains("already built in"));
    let s = session(|i| i.tools = tools(&["deploy"])).unwrap();
    let _: &[ToolSpec] = &s.inner.specs;
    assert_eq!(s.inner.specs.last().unwrap().name, "deploy");
}

#[test]
fn totals_count_requests_and_local_models() {
    let s = session(|_| {}).unwrap();
    let factory = s.state().factory.clone();
    s.count(
        &Usage { prompt_tokens: 900, completion_tokens: 10, cached_tokens: None, cost: Some(0.01) },
        factory.as_ref(),
        None,
        None,
    );
    assert_eq!(s.usage().totals.requests, 1);
    // Per model: the turn's own model, then a subagent whose type named another one (counted under that name).
    let own = factory.model().to_string();
    let usage = Usage { prompt_tokens: 10, completion_tokens: 1, cached_tokens: None, cost: Some(0.5) };
    s.count(&usage, factory.as_ref(), None, Some("other/model"));
    let totals = s.usage().totals;
    assert_eq!((totals.requests, totals.models[&own].requests, totals.models["other/model"].requests), (2, 1, 1));
    assert_eq!(totals.models["other/model"].cost, Some(0.5));
    s.state().last = Some(Usage { prompt_tokens: 900, completion_tokens: 10, cached_tokens: None, cost: None });
    assert!(s.nearly_full());
}

/// #140: compaction starts at `compactTokens` or 85% of the window, whichever comes first; off leaves the window.
#[test]
fn compact_tokens_compacts_before_the_window_would() {
    let used = |s: &MarvSession, prompt_tokens| {
        s.state().last = Some(Usage { prompt_tokens, completion_tokens: 0, cached_tokens: None, cost: None });
        s.nearly_full()
    };
    // The window (1000, so 850) comes first with the default 100k.
    let default = session(|_| {}).unwrap();
    assert!(!used(&default, 849) && used(&default, 850));
    // A budget under the window comes first.
    let budget = session(|i| i.compact_tokens = Some(500)).unwrap();
    assert!(!used(&budget, 499) && used(&budget, 500));
    // Off: only the window.
    let off = session(|i| i.compact_tokens = None).unwrap();
    assert!(!used(&off, 800) && used(&off, 850));
    // A large window (400k, so 340k) and the default: 100k comes first.
    let large = session(|i| i.provider = ProviderChoice::Option(ProviderOption::Custom(Arc::new(Window(Some(400_000))))));
    assert!(!used(&large.unwrap(), 99_999));
    let large = session(|i| i.provider = ProviderChoice::Option(ProviderOption::Custom(Arc::new(Window(Some(400_000))))));
    assert!(used(&large.unwrap(), 100_000));
    // No known window: compactTokens alone; off, never.
    let unknown = |b| {
        session(|i| {
            i.provider = ProviderChoice::Option(ProviderOption::Custom(Arc::new(Window(None))));
            i.compact_tokens = b;
        })
        .unwrap()
    };
    assert!(!used(&unknown(Some(100_000)), 99_999) && used(&unknown(Some(100_000)), 100_000));
    assert!(!used(&unknown(None), 1_000_000));
}

/// #159 review: a ChatGPT model has a window (Marv's table, or a safe default), and a Z.ai plan model too, so
/// the window rule can compact even with `compactTokens` off.
#[test]
fn plan_models_have_a_window_so_compaction_can_fire() {
    let used = |s: &MarvSession, prompt_tokens| {
        s.state().last = Some(Usage { prompt_tokens, completion_tokens: 0, cached_tokens: None, cost: None });
        s.nearly_full()
    };
    let chatgpt = |model: &str| {
        session(|i| {
            let auth_file = std::path::PathBuf::from("/nonexistent/chatgpt.json");
            i.provider = ProviderChoice::Option(ProviderOption::ChatGpt { model: model.into(), auth_file });
            i.compact_tokens = None;
        })
        .unwrap()
    };
    let unknown = chatgpt("gpt-6.1-sol");
    assert_eq!(unknown.context_length(), Some(marv_provider::chatgpt::DEFAULT_CONTEXT_WINDOW));
    assert!(!used(&unknown, 108_799) && used(&unknown, 108_800), "85% of the default window");
    assert_eq!(chatgpt("gpt-5.1-codex").context_length(), Some(272_000));
    let flash = marv_provider::models::zai_models().into_iter().find(|m| m.id == "glm-5.3-flash").unwrap();
    assert_eq!(flash.context, Some(200_000));
}

/// A provider with a given window (or none known).
struct Window(Option<u64>);
impl Provider for Window {
    fn name(&self) -> &str {
        "window"
    }
    fn context_length(&self) -> Option<u64> {
        self.0
    }
    fn stream(&self, _history: &[ChatTurn], _options: marv_provider::StreamOptions) -> EventStream {
        Box::pin(futures::stream::empty())
    }
}

#[test]
fn turn_end_reasons_use_marvs_names() {
    assert_eq!(TurnEndReason::Done(StopReason::MaxSteps).as_str(), "max_steps");
    assert_eq!(TurnEndReason::Interrupted.as_str(), "interrupted");
}
