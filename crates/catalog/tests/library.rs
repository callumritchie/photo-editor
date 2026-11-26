use pe_catalog::*;

fn library(n: u8) -> (tempfile::TempDir, Catalog, Vec<i64>) {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("p");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..n {
        image::RgbImage::from_pixel(32, 24, image::Rgb([i * 20, 90, 160]))
            .save(src.join(format!("IMG_{i:02}.jpg")))
            .unwrap();
    }
    let mut cat = Catalog::create(&t.path().join("lib.pecat")).unwrap();
    cat.import(&[src], &ImportOptions::default(), &mut |_| {})
        .unwrap();
    let ids = cat
        .images(&Query::default())
        .unwrap()
        .iter()
        .map(|i| i.id)
        .collect();
    (t, cat, ids)
}

fn ids(cat: &Catalog, q: Query) -> Vec<i64> {
    cat.images(&q).unwrap().iter().map(|i| i.id).collect()
}

fn rules(json: &str) -> RuleSet {
    serde_json::from_str(json).unwrap()
}

#[test]
fn collections_sets_and_smart_collections() {
    let (_t, mut cat, img) = library(6);
    let set = cat
        .create_collection("Trips", CollectionKind::Set, None, None)
        .unwrap();
    let c = cat
        .create_collection("Paris", CollectionKind::Collection, Some(set), None)
        .unwrap();
    cat.add_to_collection(c, &img[..3]).unwrap();
    cat.add_to_collection(c, &img[..1]).unwrap(); // idempotent
    assert_eq!(
        ids(
            &cat,
            Query {
                collection_id: Some(c),
                ..Default::default()
            }
        ),
        img[..3]
    );

    // Smart: rating >= 3 and in Paris; updates as ratings change.
    let smart = cat
        .create_collection(
            "Best of Paris",
            CollectionKind::Smart,
            Some(set),
            Some(&rules(&format!(
                r#"{{"match":"all","rules":[{{"field":"rating","op":"ge","value":3}},
                   {{"field":"collection","op":"is","value":{c}}}]}}"#
            ))),
        )
        .unwrap();
    let q = Query {
        collection_id: Some(smart),
        ..Default::default()
    };
    assert!(ids(&cat, q.clone()).is_empty());
    cat.set_rating(&[img[1], img[4]], 4).unwrap();
    assert_eq!(
        ids(&cat, q.clone()),
        vec![img[1]],
        "img[4] is rated but not in Paris"
    );

    let list = cat.collections().unwrap();
    let names: Vec<_> = list
        .iter()
        .map(|c| (c.name.as_str(), c.image_count))
        .collect();
    assert_eq!(names, [("Trips", 0), ("Best of Paris", 1), ("Paris", 3)]);

    // Rules and structure are validated.
    assert!(
        cat.create_collection("Paris", CollectionKind::Collection, Some(set), None)
            .is_err(),
        "name taken"
    );
    assert!(
        cat.create_collection("x", CollectionKind::Collection, Some(c), None)
            .is_err(),
        "parent not a set"
    );
    assert!(
        cat.add_to_collection(smart, &img[..1]).is_err(),
        "smart collections are not hand-filled"
    );
    assert!(
        cat.move_collection(set, Some(set)).is_err(),
        "not into itself"
    );
    let self_ref = rules(&format!(
        r#"{{"rules":[{{"field":"collection","op":"is","value":{smart}}}]}}"#
    ));
    assert!(
        cat.set_smart_rules(smart, &self_ref).is_err(),
        "no self reference"
    );
    assert!(cat.images(&q).is_ok(), "rejected rules are not kept");

    cat.remove_from_collection(c, &[img[1]]).unwrap();
    assert!(
        ids(&cat, q).is_empty(),
        "smart collection follows its source"
    );
    cat.delete_collection(set).unwrap();
    assert!(
        cat.collections().unwrap().is_empty(),
        "deleting a set deletes its contents"
    );
    assert_eq!(
        cat.images(&Query::default()).unwrap().len(),
        6,
        "photos untouched"
    );
}

#[test]
fn keywords_hierarchy_search_and_rules() {
    let (_t, mut cat, img) = library(4);
    cat.add_keywords(&img[..2], &["Animals > Birds > Owl".into()])
        .unwrap();
    cat.add_keywords(&[img[2]], &["Animals > Cats".into(), "Night".into()])
        .unwrap();
    let kws = cat.keywords().unwrap();
    let paths: Vec<_> = kws.iter().map(|k| k.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "Animals",
            "Animals > Birds",
            "Animals > Birds > Owl",
            "Animals > Cats",
            "Night"
        ]
    );
    let animals = kws[0].id;
    let owl = kws[2].id;
    assert_eq!(kws[2].image_count, 2);
    // Reusing a path does not duplicate it (case-insensitive).
    assert_eq!(cat.keyword_for_path("animals > birds > owl").unwrap(), owl);

    assert_eq!(
        ids(
            &cat,
            Query {
                keyword_id: Some(animals),
                ..Default::default()
            }
        ),
        img[..3],
        "includes children"
    );
    assert_eq!(
        ids(
            &cat,
            Query {
                text: Some("owl".into()),
                ..Default::default()
            }
        ),
        img[..2]
    );
    let birds = rules(r#"{"rules":[{"field":"keyword","op":"is","value":"Birds"}]}"#);
    assert_eq!(
        ids(
            &cat,
            Query {
                rules: Some(birds),
                ..Default::default()
            }
        ),
        img[..2]
    );
    let none = rules(r#"{"rules":[{"field":"keyword","op":"empty"}]}"#);
    assert_eq!(
        ids(
            &cat,
            Query {
                rules: Some(none),
                ..Default::default()
            }
        ),
        vec![img[3]]
    );

    let of = cat.keywords_of(&img[..3]).unwrap();
    assert!(of.iter().any(|k| k.id == owl && k.image_count == 2));
    cat.remove_keyword(&[img[0]], owl).unwrap();
    assert_eq!(
        ids(
            &cat,
            Query {
                keyword_id: Some(owl),
                ..Default::default()
            }
        ),
        vec![img[1]]
    );
    cat.delete_keyword(animals).unwrap();
    assert_eq!(cat.keywords().unwrap().len(), 1, "children deleted too");
}

#[test]
fn rule_operators() {
    let (_t, mut cat, img) = library(5);
    cat.set_rating(&img[..2], 5).unwrap();
    cat.set_flag(&[img[0]], Flag::Picked).unwrap();
    cat.set_label(&[img[4]], Some("red")).unwrap();
    cat.develop_execute(
        &[img[3]],
        &DevelopCommand::Set {
            settings: Settings {
                exposure: 1.0,
                ..Default::default()
            },
            label: None,
        },
    )
    .unwrap();
    let q = |json: &str| {
        ids(
            &cat,
            Query {
                rules: Some(rules(json)),
                ..Default::default()
            },
        )
    };
    assert_eq!(
        q(r#"{"rules":[{"field":"flag","op":"is","value":"picked"}]}"#),
        vec![img[0]]
    );
    assert_eq!(
        q(r#"{"rules":[{"field":"label","op":"is","value":"red"}]}"#),
        vec![img[4]]
    );
    assert_eq!(
        q(r#"{"rules":[{"field":"edited","op":"is","value":true}]}"#),
        vec![img[3]]
    );
    assert_eq!(
        q(r#"{"rules":[{"field":"fileName","op":"endsWith","value":"03.jpg"}]}"#),
        vec![img[3]]
    );
    assert_eq!(
        q(r#"{"rules":[{"field":"rating","op":"between","value":4,"value2":5}]}"#),
        img[..2]
    );
    assert_eq!(
        q(r#"{"rules":[{"field":"fileType","op":"is","value":"raw"}]}"#),
        Vec::<i64>::new()
    );
    // any / none / nesting
    assert_eq!(
        q(
            r#"{"match":"any","rules":[{"field":"label","op":"is","value":"red"},{"field":"flag","op":"is","value":"picked"}]}"#
        ),
        vec![img[0], img[4]]
    );
    assert_eq!(
        q(
            r#"{"match":"none","rules":[{"field":"rating","op":"ge","value":1},
             {"match":"all","rules":[{"field":"label","op":"notEmpty"}]}]}"#
        ),
        vec![img[2], img[3]]
    );
    // Wildcards in text are literal.
    assert!(q(r#"{"rules":[{"field":"fileName","op":"contains","value":"%"}]}"#).is_empty());
    // Bad operator for a field is an error, not a silent match.
    assert!(cat
        .images(&Query {
            rules: Some(rules(r#"{"rules":[{"field":"flag","op":"gt","value":1}]}"#)),
            ..Default::default()
        })
        .is_err());
}

#[test]
fn virtual_copies_have_their_own_edits() {
    let (_t, mut cat, img) = library(2);
    cat.develop_execute(
        &[img[0]],
        &DevelopCommand::Set {
            settings: Settings {
                exposure: 0.5,
                ..Default::default()
            },
            label: None,
        },
    )
    .unwrap();
    cat.add_keywords(&[img[0]], &["Tag".into()]).unwrap();
    let copy = cat.create_virtual_copy(img[0]).unwrap();
    let second = cat.create_virtual_copy(copy).unwrap();
    let all = cat.images(&Query::default()).unwrap();
    assert_eq!(
        all.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![img[0], copy, second, img[1]],
        "copies follow their original"
    );
    let c = all.iter().find(|i| i.id == copy).unwrap();
    assert_eq!(
        (c.copy_name.as_deref(), c.master_id),
        (Some("Copy 1"), Some(img[0]))
    );
    assert_eq!(
        all.iter().find(|i| i.id == second).unwrap().master_id,
        Some(img[0]),
        "copy of a copy belongs to the original"
    );
    assert_eq!(cat.develop_settings(copy).unwrap().exposure, 0.5);
    cat.develop_execute(
        &[copy],
        &DevelopCommand::Set {
            settings: Settings {
                exposure: -1.0,
                ..Default::default()
            },
            label: None,
        },
    )
    .unwrap();
    assert_eq!(
        cat.develop_settings(img[0]).unwrap().exposure,
        0.5,
        "original unchanged"
    );
    assert_eq!(
        cat.keywords_of(&[copy]).unwrap().len(),
        1,
        "keywords copied"
    );
    let vc = rules(r#"{"rules":[{"field":"virtualCopy","op":"is","value":true}]}"#);
    assert_eq!(
        ids(
            &cat,
            Query {
                rules: Some(vc),
                ..Default::default()
            }
        ),
        vec![copy, second]
    );

    assert_eq!(cat.remove_images(&[img[0]]).unwrap(), 1);
    assert_eq!(
        ids(&cat, Query::default()),
        vec![img[1]],
        "removing the original removes its copies"
    );
    assert_eq!(cat.integrity_check().unwrap(), "ok");
}

#[test]
fn stacks_collapse_and_reorder() {
    let (_t, mut cat, img) = library(5);
    let sid = cat.stack(&[img[2], img[1], img[3]]).unwrap();
    let collapsed = Query {
        collapse_stacks: true,
        ..Default::default()
    };
    let shown = cat.images(&collapsed).unwrap();
    assert_eq!(
        shown.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![img[0], img[2], img[4]]
    );
    let top = shown.iter().find(|i| i.id == img[2]).unwrap();
    assert_eq!(
        (top.stack_size, top.stack_position, top.stack_collapsed),
        (3, Some(0), true)
    );

    cat.set_stack_collapsed(sid, false).unwrap();
    assert_eq!(cat.images(&collapsed).unwrap().len(), 5);
    cat.set_stack_collapsed(sid, true).unwrap();
    cat.set_stack_top(img[3]).unwrap();
    assert!(ids(&cat, collapsed.clone()).contains(&img[3]));

    cat.unstack(&[img[3]]).unwrap();
    let rows = cat.images(&Query::default()).unwrap();
    let pos = |id| rows.iter().find(|i| i.id == id).unwrap().stack_position;
    assert_eq!(
        (pos(img[2]), pos(img[1]), pos(img[3])),
        (Some(0), Some(1), None),
        "renumbered from 0"
    );
    cat.unstack(&[img[1]]).unwrap();
    assert!(
        cat.images(&Query::default())
            .unwrap()
            .iter()
            .all(|i| i.stack_id.is_none()),
        "one photo is not a stack"
    );
    assert!(cat.stack(&[img[0]]).is_err());
}
