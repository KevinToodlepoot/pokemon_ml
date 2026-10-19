//! Every part a regulation the engine plays allows has its behaviour (D65): no move, ability or
//! item listed in the tables without it (`supported: false`, Commander's case) is legal in either
//! format file, nor in the oracle's pool (`oracle/pool.json`, what `gen_cases.js` and the
//! mutation tier draw from); and every Mega such a format allows has a modelled ability. A new
//! regulation or a Showdown bump that makes one legal fails here by name, before any battle is
//! played with it.

use vgc_engine::data::{ABILITIES, ITEMS, MOVES, SPECIES, ability_id, item_id, move_id, species_id};

fn json(path: &str) -> serde_json::Value {
    let path = format!("{}/{path}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v.as_array().map_or(Vec::new(), |a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

/// What a list of legal species, abilities, moves and items holds that the engine does not model.
fn unmodelled(species: &[serde_json::Value], items: &[String], abilities: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, id: &str, known: Option<bool>| match known {
        Some(true) => {}
        Some(false) => out.push(format!("{what} {id} has no behaviour")),
        None => out.push(format!("{what} {id} is not in the tables")),
    };
    for s in species {
        let id = s["id"].as_str().unwrap();
        check("species", id, species_id(id).map(|_| true));
        for m in ids(&s["moves"]) {
            check("move", &m, move_id(&m).map(|k| MOVES[k as usize].supported));
        }
        for a in ids(&s["abilities"]) {
            check("ability", &a, ability_id(&a).map(|k| ABILITIES[k as usize].supported));
        }
        // A Mega has the one ability, its species' first.
        for stone in ids(&s["megas"]) {
            check("mega stone", &stone, item_id(&stone).map(|k| ITEMS[k as usize].supported));
            for &(_, to) in item_id(&stone).map_or(&[][..], |k| ITEMS[k as usize].mega) {
                let a = SPECIES[to as usize].ability0;
                check(&format!("{}'s ability", SPECIES[to as usize].id), ABILITIES[a as usize].id, Some(ABILITIES[a as usize].supported));
            }
        }
    }
    for i in items {
        check("item", i, item_id(i).map(|k| ITEMS[k as usize].supported));
    }
    for a in abilities {
        check("ability", a, ability_id(a).map(|k| ABILITIES[k as usize].supported));
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_part_the_regulations_allow_has_its_behaviour() {
    for file in ["formats/gen9championsvgc2026regmc.json", "formats/gen9championsvgc2026regmb.json"] {
        let f = json(file);
        let species = f["species"].as_array().unwrap();
        assert!(species.len() > 200, "{file}: {} species", species.len());
        let missing = unmodelled(species, &ids(&f["items"]), &[]);
        assert!(missing.is_empty(), "{file}: {missing:#?}");
    }
}

#[test]
fn every_part_the_oracle_draws_from_has_its_behaviour() {
    let p = json("oracle/pool.json");
    let species = p["species"].as_array().unwrap();
    let mut items = ids(&p["items"]);
    items.extend(ids(&p["megastones"]));
    let missing = unmodelled(species, &items, &ids(&p["abilities"]));
    assert!(missing.is_empty(), "oracle/pool.json: {missing:#?}");
}

#[test]
fn a_part_without_behaviour_is_caught() {
    // Commander: listed (Tatsugiri's), no behaviour, legal nowhere the engine plays.
    let fake = serde_json::json!([{ "id": "venusaur", "moves": ["protect"], "abilities": ["commander"], "megas": [] }]);
    let missing = unmodelled(fake.as_array().unwrap(), &[], &[]);
    assert_eq!(missing, ["ability commander has no behaviour"]);
}
