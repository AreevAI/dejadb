//! M2 exit test: CAL text → executor → DejaDbFacade → embedded Turso store.
//!
//! Covers the read tier (RECALL, EXISTS, HISTORY, pipeline COUNT) and the
//! ADD tier (ADD, SUPERSEDE) end-to-end against a real memory file.

use dejadb_cal::executor::CalResultPayload;
use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_store::DejaDB;
use tempfile::TempDir;

fn setup() -> (CalExecutor, DejaDbFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = DejaDbFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn added_hash(payload: &CalResultPayload) -> String {
    match payload {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        CalResultPayload::Superseded { new_hash, .. } => new_hash.clone(),
        other => panic!("expected Added/Superseded payload, got: {other:?}"),
    }
}

#[test]
fn cal_add_then_recall() {
    let (ex, facade, _d) = setup();

    let add = ex
        .execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "integration""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);
    assert_eq!(hash.len(), 64);

    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "john""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "rust");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_recall_recent_experience_without_subject() {
    // The "reflect over recent experience" path: RECALL by grain type with no
    // subject/free-text anchor now does a bounded recent-scan (it used to
    // error). Observations carry observer_id, filtered as a post-condition.
    let (ex, facade, _d) = setup();
    for (obs_id, body) in [
        ("executor", "attempt 1 failed"),
        ("executor", "attempt 2 passed"),
        ("planner", "unrelated note"),
    ] {
        ex.execute(
            &format!(
                r#"ADD observation SET observer_id = "{obs_id}" SET observer_type = "llm" SET content = "{body}" SET namespace = "caller" REASON "log""#
            ),
            &facade,
        )
        .unwrap();
    }

    // Bare recent-by-type scan returns all three, newest first.
    let all = ex.execute("RECALL observations RECENT 10", &facade).unwrap();
    match all.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 3),
        other => panic!("expected Grains, got {other:?}"),
    }

    // observer_id post-filter narrows to the two executor observations.
    let filtered = ex
        .execute(
            r#"RECALL observations WHERE observer_id = "executor" RECENT 10"#,
            &facade,
        )
        .unwrap();
    match filtered.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 2),
        other => panic!("expected Grains, got {other:?}"),
    }

    // A wildcard recall with no anchor at all is still rejected as too broad.
    assert!(ex.execute("RECALL * RECENT 10", &facade).is_err());
}

#[test]
fn cal_exists_after_add() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "a" SET relation = "r" SET object = "o" SET namespace = "caller" REASON "t""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);

    let q = format!("EXISTS sha256:{hash}");
    let res = ex.execute(&q, &facade).unwrap();
    match res.result {
        CalResultPayload::Exists { exists, .. } => assert!(exists),
        other => panic!("expected Exists, got {other:?}"),
    }
}

#[test]
fn cal_supersede_and_history() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "acct" SET relation = "balance" SET object = "100" SET namespace = "caller" REASON "init""#,
            &facade,
        )
        .unwrap();
    let h1 = added_hash(&add.result);

    let sup = ex
        .execute(
            &format!(r#"SUPERSEDE sha256:{h1} SET object = "80" REASON "withdrawal""#),
            &facade,
        )
        .unwrap();
    let h2 = added_hash(&sup.result);
    assert_ne!(h1, h2);

    // Current recall sees only the new version.
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "acct""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "80");
        }
        other => panic!("expected Grains, got {other:?}"),
    }

    // HISTORY walks the chain, newest first.
    let hist = ex
        .execute(
            r#"HISTORY WHERE subject = "acct" AND relation = "balance""#,
            &facade,
        )
        .unwrap();
    match hist.result {
        CalResultPayload::History { versions } => {
            assert_eq!(versions.len(), 2);
            let v = serde_json::to_value(&versions).unwrap();
            assert_eq!(v[0]["object"], "80");
            assert_eq!(v[1]["object"], "100");
        }
        other => panic!("expected History, got {other:?}"),
    }
}

#[test]
fn cal_pipeline_count() {
    let (ex, facade, _d) = setup();
    for i in 0..3 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "kid" SET relation = "likes" SET object = "toy{i}" SET namespace = "caller" REASON "t""#
            ),
            &facade,
        )
        .unwrap();
    }
    let res = ex
        .execute(r#"RECALL facts WHERE subject = "kid" | COUNT"#, &facade)
        .unwrap();
    match res.result {
        CalResultPayload::Count { count } => assert_eq!(count, 3),
        other => panic!("expected Count, got {other:?}"),
    }
}

#[test]
fn destructive_tokens_are_parse_errors() {
    let (ex, facade, _d) = setup();
    for q in ["DELETE sha256:abc", r#"DROP TABLE grains"#] {
        assert!(ex.execute(q, &facade).is_err(), "{q} must not execute");
    }
}

#[test]
fn cal_add_inherits_session_namespace() {
    // ADD without `SET namespace` must land in the session namespace so the
    // same session's RECALL can see it (RECALL already scoped to the session).
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" REASON "session ns""#,
        &facade,
    )
    .unwrap();
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "session RECALL must see the session ADD");
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["namespace"], "caller");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_add_explicit_namespace_still_wins() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" SET namespace = "other" REASON "explicit ns""#,
        &facade,
    )
    .unwrap();
    // Not visible in the session namespace…
    let in_session = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match in_session.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
    // …but present where the user explicitly put it.
    let in_other = ex
        .execute(
            r#"RECALL facts WHERE namespace = "other" AND subject = "zoe""#,
            &facade,
        )
        .unwrap();
    match in_other.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
        other => panic!("expected Grains, got {other:?}"),
    }
}

/// The CAL reference documents `FORMAT [json AS data, markdown AS readable]`,
/// but DATA/READABLE/COMPACT are keyword tokens — parsing the alias as a bare
/// identifier rejected the manual's own example. End-to-end: the aliases must
/// survive to the rendered output and key the format map.
#[test]
fn cal_format_aliases_may_be_reserved_words() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let res = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" FORMAT [json AS data, markdown AS readable]"#,
            &facade,
        )
        .unwrap();
    match res.result {
        CalResultPayload::MultiFormatted { formats, .. } => {
            let mut keys: Vec<_> = formats.keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, vec!["data".to_string(), "readable".to_string()]);
            assert!(!formats["data"].is_empty(), "json rendering is empty");
            assert!(!formats["readable"].is_empty(), "markdown rendering is empty");
        }
        other => panic!("expected MultiFormatted, got: {other:?}"),
    }
}

/// `RECALL *` is the documented explicit spelling of "any grain type"; it must
/// parse, execute, and mean exactly what omitting the grain type means.
///
/// Note what this does *not* assert. On this build `RECALL *` cannot actually
/// return a non-fact grain: the hexastore subject index and the text index
/// cover fact triples only, and the unanchored forms (`RECALL * LIMIT n`,
/// `RECALL * RECENT n`) are refused because they need a *specific* type. So
/// `*` is currently indistinguishable from `facts` in its results — the
/// syntax carries the right "no type filter" semantics and will widen on its
/// own if the store ever indexes other types.
#[test]
fn cal_recall_star_means_no_grain_type_filter() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        r#"ADD observation SET subject = "john" SET content = "asked about pricing" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let starred = ex
        .execute(r#"RECALL * WHERE subject = "john""#, &facade)
        .expect("RECALL * must execute");
    let omitted = ex
        .execute(r#"RECALL WHERE subject = "john""#, &facade)
        .expect("typeless RECALL must execute");
    match (starred.result, omitted.result) {
        (
            CalResultPayload::Grains { grains: a, .. },
            CalResultPayload::Grains { grains: b, .. },
        ) => {
            assert_eq!(
                a.len(),
                b.len(),
                "RECALL * must mean the same as omitting the grain type"
            );
            assert!(!a.is_empty(), "expected the seeded fact");
        }
        other => panic!("expected two Grains payloads, got: {other:?}"),
    }
}

/// The anti-full-scan guard stays: with neither a subject, a free-text query,
/// nor a *specific* grain type there is nothing to anchor the scan. The CAL
/// reference's `RECALL * WHERE namespace = ...` example cannot execute, and
/// that is deliberate — do not relax this to make a snippet true.
#[test]
fn cal_recall_star_still_needs_an_anchor() {
    let (ex, facade, _d) = setup();
    let err = ex
        .execute(r#"RECALL * WHERE namespace = "caller""#, &facade)
        .expect_err("unanchored RECALL * must be refused");
    assert_eq!(err.code(), "CAL-E092", "unexpected error: {err:?}");
}

// ── Saved queries & custom templates ────────────────────────────────────
//
// These are host metadata carried by the file, not memories. The point of
// persisting them is that they survive the process — so the tests reopen.

fn facade_at(path: &std::path::Path) -> DejaDbFacade {
    let m = DejaDB::open(path.to_str().unwrap()).unwrap();
    DejaDbFacade::with_session(m, Some("caller".to_string()), None)
}

#[test]
fn saved_query_round_trips_through_the_file() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
            &facade,
        )
        .unwrap();
        ex.execute(
            r#"DEFINE QUERY "john brief" DESCRIPTION "what we know" AS { RECALL facts WHERE subject = "john" LIMIT 20 }"#,
            &facade,
        )
        .expect("DEFINE QUERY must succeed");
        assert!(
            facade.list_queries().iter().any(|q| q.name == "john brief"),
            "the query should be listed in the defining process"
        );
    }

    // A new process over the same file must still see it, and be able to run it.
    let facade = facade_at(&path);
    let saved = facade
        .get_query("john brief")
        .expect("saved query must survive reopen");
    assert_eq!(saved.description, "what we know");
    assert!(!saved.builtin);

    let run = ex.execute(r#"RUN "john brief""#, &facade).expect("RUN must work");
    match run.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
        other => panic!("expected Grains from RUN, got: {other:?}"),
    }

    // RUN records last_run_at, and that too is persisted.
    let after = facade_at(&path).get_query("john brief").unwrap();
    assert!(
        after.last_run_at.is_some(),
        "RUN should have recorded last_run_at on disk"
    );

    // DROP removes it from the file, not just from memory.
    ex.execute(r#"DROP QUERY "john brief""#, &facade)
        .expect("DROP QUERY must succeed");
    assert!(facade_at(&path).get_query("john brief").is_none());
}

/// A file can carry host metadata this build cannot load — a template written
/// before the §10.8 body limit was enforced, a row from a newer version.
/// Skipping it is right (one bad row must not make the memory unusable), but
/// skipping it *silently* is not: the operator would find out by noticing a
/// template had gone missing.
#[test]
fn a_template_the_file_carries_but_this_build_cannot_load_is_reported() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");

    // Write the row the way an older build would have: straight into `meta`,
    // with a body past the limit this build enforces.
    {
        let m = DejaDB::open(path.to_str().unwrap()).unwrap();
        let oversized = "x".repeat(dejadb_cal::templates::MAX_TEMPLATE_SIZE + 1);
        let row = serde_json::json!({
            "source": oversized,
            "description": "written by an older build",
            "parent": null,
            "grain_types": [],
        });
        m.meta_put("tpl:legacy", &row.to_string()).unwrap();
    }

    let facade = facade_at(&path);
    assert!(
        facade.get_template("legacy").is_none(),
        "an unloadable template must not be served as if it were fine"
    );
    let warnings = facade.meta_warnings();
    assert!(
        warnings.iter().any(|w| w.contains("legacy")),
        "the drop must be reported, got: {warnings:?}"
    );
}

/// `DEFINE TEMPLATE` names go through the label parser, so plenty of ordinary
/// words that happen to be CAL keywords are valid names. `FORMAT TEMPLATE` used
/// to accept only a bare identifier, which made those templates definable but
/// unreferenceable — you could write one and never use it.
#[test]
fn a_template_named_after_a_keyword_can_still_be_referenced() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "seed""#,
        &facade,
    )
    .unwrap();

    // Keyword tokens, but not names the built-in presets already hold.
    for name in ["recent", "scope", "priority"] {
        ex.execute(
            &format!(r#"DEFINE TEMPLATE {name} AS "[{name}] {{{{grain.content}}}}""#),
            &facade,
        )
        .unwrap_or_else(|e| panic!("DEFINE TEMPLATE {name} should parse: {e}"));

        let out = ex
            .execute(
                &format!(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE {name}"#),
                &facade,
            )
            .unwrap_or_else(|e| panic!("FORMAT TEMPLATE {name} should resolve: {e}"));
        match out.result {
            CalResultPayload::Formatted { text, .. } => {
                assert!(text.contains(&format!("[{name}]")), "wrong template ran: {text}")
            }
            other => panic!("expected Formatted, got: {other:?}"),
        }
    }
}

/// The `FOR` clause rides the statement, not the template body, so it only
/// survives a reopen if it is put back explicitly — it used to be written to
/// the file and then dropped on the way back in.
#[test]
fn a_templates_for_clause_survives_a_reopen() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"DEFINE TEMPLATE facts_only FOR facts, goals AS "{{grain.content}}""#,
            &facade,
        )
        .expect("DEFINE TEMPLATE ... FOR must succeed");
        assert_eq!(
            facade.get_template("facts_only").unwrap().grain_types,
            vec!["facts".to_string(), "goals".to_string()],
            "the FOR clause must be visible in the defining process too"
        );
    }

    let reopened = facade_at(&path).get_template("facts_only").unwrap();
    assert_eq!(
        reopened.grain_types,
        vec!["facts".to_string(), "goals".to_string()],
        "the FOR clause must survive the reopen"
    );
}

#[test]
fn custom_template_round_trips_and_renders() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        ex.execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
            &facade,
        )
        .unwrap();
        ex.execute(
            r#"DEFINE TEMPLATE "brief" DESCRIPTION "one line each" AS "{{#each grains}}- {{subject}} {{relation}} {{object}}
{{/each}}""#,
            &facade,
        )
        .expect("DEFINE TEMPLATE must succeed");
    }

    let facade = facade_at(&path);
    assert!(
        facade.list_templates().iter().any(|t| t.name == "brief" && !t.builtin),
        "custom template must survive reopen"
    );
    let rendered = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" FORMAT preset "brief""#,
            &facade,
        )
        .expect("preset render must work");
    match rendered.result {
        CalResultPayload::Formatted { text, .. } => {
            assert!(text.contains("john likes rust"), "unexpected render: {text:?}");
        }
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// The registry owns the rules; a rejected definition must not reach the file.
#[test]
fn an_invalid_saved_query_name_is_not_persisted() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let facade = facade_at(&path);
    // Leading digit violates the name rule.
    assert!(facade.define_query("9bad", "RECALL facts", None, &[]).is_err());
    assert!(facade_at(&path).get_query("9bad").is_none());
}

// ── Sectioned templates (OMS CAL §10.6) ─────────────────────────────────────

/// A sectioned definition must survive a real process reopen and render
/// through the engine-driven pipeline: HEADER once, ELEMENT per grain,
/// FOOTER once.
#[test]
fn sectioned_template_round_trips_and_renders() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());

    {
        let facade = facade_at(&path);
        for (rel, obj) in [("likes", "rust"), ("prefers", "dark mode")] {
            ex.execute(
                &format!(
                    r#"ADD fact SET subject = "john" SET relation = "{rel}" SET object = "{obj}" REASON "seed""#
                ),
                &facade,
            )
            .unwrap();
        }
        ex.execute(
            "DEFINE TEMPLATE roster\n\
             HEADER {\n\
             <context>\n\
             }\n\
             ELEMENT {\n\
             <{{grain.type}}>{{grain.content}}</{{grain.type}}>\n\
             }\n\
             FOOTER {\n\
             </context>\n\
             }",
            &facade,
        )
        .expect("sectioned DEFINE TEMPLATE must succeed");
    }

    // A new process over the same file recovers the sectioned form.
    let facade = facade_at(&path);
    let info = facade
        .get_template("roster")
        .expect("sectioned template must survive reopen");
    assert!(info.source.contains("ELEMENT {"), "source: {:?}", info.source);

    let rendered = ex
        .execute(
            r#"RECALL facts WHERE subject = "john" ORDER BY created_at ASC FORMAT preset "roster""#,
            &facade,
        )
        .expect("sectioned render must work");
    match rendered.result {
        CalResultPayload::Formatted { text, .. } => {
            assert_eq!(
                text,
                "<context>\n\
                 <fact>likes rust</fact>\n\
                 <fact>prefers dark mode</fact>\n\
                 </context>",
                "sectioned render shape"
            );
        }
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// §10.6.1 defines the shorthand by equivalence, so the two spellings must
/// produce byte-identical output.
#[test]
fn element_shorthand_equals_an_element_section() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();

    ex.execute(
        r#"DEFINE TEMPLATE shorthand AS "{{grain.subject}}: {{grain.content}}""#,
        &facade,
    )
    .expect("shorthand definition");
    ex.execute(
        "DEFINE TEMPLATE sectioned\nELEMENT {\n{{grain.subject}}: {{grain.content}}\n}",
        &facade,
    )
    .expect("sectioned definition");

    let render = |name: &str| match ex
        .execute(
            &format!(r#"RECALL facts WHERE subject = "john" FORMAT preset "{name}""#),
            &facade,
        )
        .unwrap()
        .result
    {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert_eq!(render("shorthand"), render("sectioned"));
    assert_eq!(render("shorthand"), "john: likes rust");
}

/// §7 spells template names as bare identifiers; the quoted form stays
/// accepted for names that need a space.
#[test]
fn template_names_may_be_bare_or_quoted() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(r#"DEFINE TEMPLATE bare AS "{{grain.content}}""#, &facade)
        .expect("bare identifier name");
    ex.execute(r#"DEFINE TEMPLATE "two words" AS "{{grain.content}}""#, &facade)
        .expect("quoted name");

    let names: Vec<String> = facade.list_templates().into_iter().map(|t| t.name).collect();
    assert!(names.contains(&"bare".to_string()), "{names:?}");
    assert!(names.contains(&"two words".to_string()), "{names:?}");
}

/// The reason section bodies are lexed as raw text: template prose is not
/// CAL, and before the raw capture each of these failed the whole query.
#[test]
fn a_section_body_may_contain_text_that_is_not_cal() {
    use dejadb_cal::facade::CalStoreFacade;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        "DEFINE TEMPLATE prose\nELEMENT {\ndon't DELETE this — 50% \"kept\"\n}",
        &facade,
    )
    .expect("a body that is not CAL must still define");

    let info = facade.get_template("prose").unwrap();
    assert!(
        info.source.contains("don't DELETE this — 50% \"kept\""),
        "body was mangled: {:?}",
        info.source
    );
}

/// The three §10.6 `FORMAT TEMPLATE` forms are told apart by token class:
/// bare = registered name, quoted = ELEMENT shorthand, braced = sections.
#[test]
fn format_template_has_three_forms() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        "DEFINE TEMPLATE named\nELEMENT {\n[{{grain.subject}}] {{grain.content}}\n}",
        &facade,
    )
    .unwrap();

    let render = |q: &str| match ex.execute(q, &facade).expect(q).result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted from {q:?}, got: {other:?}"),
    };

    // Bare identifier — the registered template.
    assert_eq!(
        render(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE named"#),
        "[john] likes rust"
    );
    // Quoted string — inline ELEMENT shorthand.
    assert_eq!(
        render(r#"RECALL facts WHERE subject = "john" FORMAT TEMPLATE "[{{grain.subject}}] {{grain.content}}""#),
        "[john] likes rust"
    );
    // Braced — inline sections.
    assert_eq!(
        render(
            "RECALL facts WHERE subject = \"john\" \
             FORMAT TEMPLATE { ELEMENT {\n[{{grain.subject}}] {{grain.content}}\n} }"
        ),
        "[john] likes rust"
    );
}

/// The §10.1.1 example the spec grammar previously forbade, now that
/// `format_spec` admits `"TEMPLATE" , string_literal`.
#[test]
fn spec_10_1_1_aliased_inline_template_example_runs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "prefers" SET object = "dark mode" REASON "seed""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts FORMAT [json AS structured, TEMPLATE "{{grain.subject}}: {{grain.object}}" AS oneliner]"#,
            &facade,
        )
        .expect("the spec's own example must run");
    match out.result {
        CalResultPayload::MultiFormatted { formats, .. } => {
            assert_eq!(
                formats.get("oneliner").map(String::as_str),
                Some("alice: dark mode"),
                "formats: {formats:?}"
            );
            assert!(formats.contains_key("structured"), "formats: {formats:?}");
        }
        other => panic!("expected MultiFormatted, got: {other:?}"),
    }
}

// ── §10.5 assembly/source/budget variables + §10.7 inheritance ──────────────

/// The spec's own `semantic_sml` template (§10.6), over a two-source
/// ASSEMBLE: HEADER once with the assembly intent, ELEMENT per grain,
/// SOURCE_BREAK between sources, FOOTER once.
#[test]
fn spec_semantic_sml_renders_over_a_multi_source_assemble() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for (rel, obj) in [("prefers", "dark mode"), ("works at", "acme")] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "{rel}" SET object = "{obj}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    ex.execute(
        r#"ADD goal SET subject = "alice" SET object = "ship Q1 review" REASON "seed""#,
        &facade,
    )
    .unwrap();

    ex.execute(
        "DEFINE TEMPLATE semantic_sml EXTENDS structured\n\
         HEADER {\n\
         <context intent=\"{{assembly.intent}}\" sources=\"{{assembly.source_count}}\">\n\
         }\n\
         ELEMENT {\n\
         <{{grain.type}} subject=\"{{grain.subject}}\">{{grain.content}}</{{grain.type}}>\n\
         }\n\
         SOURCE_BREAK {\n\
         \n\
         }\n\
         FOOTER {\n\
         </context>\n\
         }",
        &facade,
    )
    .expect("EXTENDS structured must resolve");

    let out = ex
        .execute(
            r#"ASSEMBLE brief FOR "helping alice" FROM f: (RECALL facts WHERE subject = "alice"), g: (RECALL goals RECENT 5) BUDGET 3000 tokens FORMAT TEMPLATE semantic_sml"#,
            &facade,
        )
        .expect("assemble render");

    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert!(
        text.starts_with("<context intent=\"helping alice\" sources=\"2\">"),
        "HEADER must carry assembly.* — got:\n{text}"
    );
    assert!(text.ends_with("</context>"), "FOOTER missing:\n{text}");
    assert!(text.contains("<goal subject=\"alice\">ship Q1 review</goal>"), "{text}");
    assert!(
        text.contains("<fact subject=\"alice\">prefers dark mode</fact>"),
        "{text}"
    );
    // SOURCE_BREAK is an empty body between the two sources, so exactly one
    // blank line separates the fact group from the goal group.
    assert!(text.contains("</fact>\n\n<goal"), "no SOURCE_BREAK between sources:\n{text}");
}

/// Regression: a *single*-source `ASSEMBLE` rendered with an empty plan, so
/// `assembly.*`, `budget.*` and `source.*` silently came back blank — bound on
/// the multi-source path and empty here, for no reason a user could see.
///
/// The single-source path budgets by grain count rather than tokens, so
/// `budget.unit` says `grains`; mislabelling it `tokens` would be a lie about
/// what the number means.
#[test]
fn assembly_variables_are_bound_for_a_single_source_assemble() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for obj in ["dark mode", "window seats", "oat milk"] {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "prefers" SET object = "{obj}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }

    ex.execute(
        "DEFINE TEMPLATE one_source\n\
         HEADER {\n\
         [{{assembly.name}}|{{assembly.intent}}] {{budget.used}}/{{budget.total}} {{budget.unit}}\n\
         }\n\
         ELEMENT {\n\
         - [{{source.index}}] {{grain.content}}\n\
         }\n\
         ELEMENT_OMIT {\n\
         - (dropped: {{grain.content}})\n\
         }",
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"ASSEMBLE brief FOR "helping alice" FROM (RECALL facts WHERE subject = "alice") BUDGET 2 tokens FORMAT TEMPLATE one_source"#,
            &facade,
        )
        .expect("single-source assemble render");

    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    assert!(
        text.starts_with("[brief|helping alice] 2/2 grains"),
        "assembly.*/budget.* must be bound for one source too — got:\n{text}"
    );
    assert!(text.contains("- [0] "), "source.index unbound:\n{text}");
    assert!(
        text.contains("(dropped: "),
        "the budget cut a grain, so ELEMENT_OMIT must account for it:\n{text}"
    );
}

/// `source.*` is scoped to the element run: bound inside ELEMENT, unbound in
/// HEADER, where §10.5 offers only the assembly-level namespace.
#[test]
fn source_variables_are_scoped_to_the_element_run() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        "DEFINE TEMPLATE labelled\n\
         HEADER {\n\
         head:[{{source.label}}] budget:{{budget.total}}\n\
         }\n\
         ELEMENT {\n\
         [{{source.label}}#{{source.index}}] {{grain.content}}\n\
         }",
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"ASSEMBLE b FOR "x" FROM f: (RECALL facts WHERE subject = "alice") BUDGET 500 tokens FORMAT TEMPLATE labelled"#,
            &facade,
        )
        .unwrap();
    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };
    // HEADER: source.label unbound (empty), budget.total bound.
    assert!(text.starts_with("head:[] budget:500"), "{text}");
    // ELEMENT: source.label bound.
    assert!(text.contains("[f#0] likes rust"), "{text}");
}

/// The same template must still render for a bare RECALL, with the
/// assembly-only variables resolving to empty rather than erroring (§10.8).
#[test]
fn assembly_variables_are_empty_for_a_bare_recall() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    ex.execute(
        r#"ADD fact SET subject = "alice" SET relation = "likes" SET object = "rust" REASON "seed""#,
        &facade,
    )
    .unwrap();
    ex.execute(
        r#"DEFINE TEMPLATE mixed AS "[{{assembly.intent}}|{{source.label}}] {{grain.content}}""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" FORMAT TEMPLATE mixed"#,
            &facade,
        )
        .unwrap();
    match out.result {
        CalResultPayload::Formatted { text, .. } => assert_eq!(text, "[|] likes rust"),
        other => panic!("expected Formatted, got: {other:?}"),
    }
}

/// `ELEMENT_OMIT` renders the grains the budget dropped. Previously
/// unreachable: the assembler trimmed them before the renderer ran, so the
/// section parsed and inherited but could never fire.
#[test]
fn element_omit_renders_grains_the_budget_dropped() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for i in 0..8 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "note{i}" SET object = "a reasonably long value so the token estimate is not trivial {i}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }

    ex.execute(
        "DEFINE TEMPLATE with_omit\n\
         ELEMENT {\n\
         + {{grain.content}}\n\
         }\n\
         ELEMENT_OMIT {\n\
         - omitted {{grain.type}}\n\
         }",
        &facade,
    )
    .unwrap();

    // A budget far too small for 8 grains forces most of them out.
    let out = ex
        .execute(
            r#"ASSEMBLE b FOR "x" FROM f: (RECALL facts WHERE subject = "alice") BUDGET 20 tokens FORMAT TEMPLATE with_omit"#,
            &facade,
        )
        .unwrap();
    let text = match out.result {
        CalResultPayload::Formatted { text, .. } => text,
        other => panic!("expected Formatted, got: {other:?}"),
    };

    let kept = text.lines().filter(|l| l.starts_with('+')).count();
    let omitted = text.lines().filter(|l| l.starts_with("- omitted")).count();
    assert!(omitted > 0, "budget should have dropped grains:\n{text}");
    assert_eq!(kept + omitted, 8, "every grain accounted for:\n{text}");
}

/// §10.8 caps `{{#each}}` at 200. The cap is correct, but truncating quietly
/// would read as "these are all the grains", so it must be announced.
#[test]
fn each_iteration_cap_emits_cal_w011() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let facade = facade_at(&path);

    for i in 0..205 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "alice" SET relation = "note{i}" SET object = "v{i}" REASON "seed""#
            ),
            &facade,
        )
        .unwrap();
    }
    // A whole-result template — the only form that can trip the cap.
    ex.execute(
        r#"DEFINE TEMPLATE legacy AS "{{#each grains}}{{object}}
{{/each}}""#,
        &facade,
    )
    .unwrap();

    let out = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 205 FORMAT preset "legacy""#,
            &facade,
        )
        .unwrap();

    let text = match &out.result {
        CalResultPayload::Formatted { text, .. } => text.clone(),
        other => panic!("expected Formatted, got: {other:?}"),
    };
    assert_eq!(text.lines().count(), 200, "cap should hold");
    assert!(
        out.warnings.iter().any(|w| w.starts_with("CAL-W011")),
        "truncation must be announced, got warnings: {:?}",
        out.warnings
    );

    // A sectioned template has the engine drive iteration, so it renders
    // everything and must NOT warn.
    ex.execute(
        "DEFINE TEMPLATE sectioned\nELEMENT {\n{{grain.object}}\n}",
        &facade,
    )
    .unwrap();
    let out2 = ex
        .execute(
            r#"RECALL facts WHERE subject = "alice" LIMIT 205 FORMAT TEMPLATE sectioned"#,
            &facade,
        )
        .unwrap();
    let text2 = match &out2.result {
        CalResultPayload::Formatted { text, .. } => text.clone(),
        other => panic!("expected Formatted, got: {other:?}"),
    };
    assert_eq!(text2.lines().count(), 205, "sectioned render is not capped");
    assert!(
        !out2.warnings.iter().any(|w| w.starts_with("CAL-W011")),
        "sectioned render must not warn: {:?}",
        out2.warnings
    );
}

