//! Synthetic LongMemEval-style pipeline regressions, not a model benchmark.
use super::*;

fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("../../tests/fixtures/memory/eval.json")).unwrap()
}

async fn exercise(real: bool) {
    for fixture in fixtures() {
        let category = fixture["category"].as_str().unwrap();
        let (mut state, tmp, owner, main) = setup().await;
        if real {
            let mut cfg = state.cfg.clone();
            cfg.harness.get_mut("claude").unwrap().command =
                std::env::var("IRIS_MEMORY_EVAL_CLI").unwrap_or_else(|_| "claude".into());
            state = AppState::new(cfg, state.paths.clone(), state.db.clone()).unwrap();
            state.refresh_harnesses().await;
        }
        state
            .db
            .with(|c| {
                if let Some(initial) = fixture.get("initial") {
                    let id = note(c, initial["text"].as_str().unwrap())?;
                    if initial["expired"] == true {
                        c.execute("UPDATE memory_items SET valid_to=0 WHERE id=?1", [id])?;
                    }
                }
                for (n, session) in fixture["sessions"].as_array().unwrap().iter().enumerate() {
                    let conv = if n == 0 {
                        main.clone()
                    } else {
                        db::create_conversation(c, &owner, "side", "Garden follow-up", None)?.id
                    };
                    for text in session.as_array().unwrap() {
                        db::insert_message(
                            c,
                            &NewMessage {
                                conversation_id: &conv,
                                role: "user",
                                author_user_id: Some(&owner),
                                content: text.as_str().unwrap(),
                                ..Default::default()
                            },
                        )?;
                    }
                }
                Ok(())
            })
            .unwrap();
        if !real {
            reply(&tmp, json!({"ops":fixture["ops"]}));
        }
        run(&state, &owner, true)
            .await
            .unwrap_or_else(|e| panic!("{category}: {e}"));
        state
            .db
            .with(|c| {
                crate::memory::lifecycle::maintain(c, &state.paths, now_ms())?;
                let hits = recall::list(
                    c,
                    &recall::Filter {
                        query: Some(fixture["query"].as_str().unwrap().into()),
                        ..Default::default()
                    },
                    true,
                )?;
                let hits = hits["hits"].as_array().unwrap();
                if !real {
                    let texts: Vec<_> = hits.iter().map(|i| i["text"].clone()).collect();
                    assert_eq!(json!(texts), fixture["expected"], "{category}");
                } else if category == "abstention" {
                    assert!(hits.is_empty(), "{category}: {hits:?}");
                } else {
                    assert!(!hits.is_empty(), "{category}: no relevant recall");
                }
                for forbidden in fixture["absent"].as_array().unwrap() {
                    assert!(
                        hits.iter().all(|i| !i["text"]
                            .as_str()
                            .unwrap()
                            .to_lowercase()
                            .contains(&forbidden.as_str().unwrap().to_lowercase())),
                        "{category}: stale or invented recall"
                    );
                }
                for hit in hits {
                    let item = crate::memory::get(c, hit["id"].as_i64().unwrap())?.unwrap();
                    assert_eq!(item.source["type"], "dream", "{category}");
                    if category == "multi-session" {
                        let conversations: HashSet<_> = item.source["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|m| m["conversation_id"].as_str().unwrap())
                            .collect();
                        assert_eq!(conversations.len(), 2);
                    }
                    assert!(item.last_used_at.is_some());
                    assert!(item.use_count > 0);
                }
                assert!(watermark(c)? > 0, "{category}: no successful progress");
                Ok(())
            })
            .unwrap();
        if !real {
            let args = std::fs::read_to_string(tmp.path().join("args")).unwrap();
            assert!(args.contains("--tools\n\n"), "tool-free CLI: {category}");
            let input = std::fs::read_to_string(tmp.path().join("input")).unwrap();
            assert!(
                input.contains(fixture["sessions"][0][0].as_str().unwrap()),
                "{category}: missing source input"
            );
        }
    }
}

#[tokio::test]
async fn synthetic_memory_eval_five_categories() {
    exercise(false).await;
}

#[tokio::test]
#[ignore = "uses the owner's official Claude CLI subscription; scripts/e2e/memory/real-cli.sh"]
async fn real_cli_memory_eval() {
    exercise(true).await;
}
