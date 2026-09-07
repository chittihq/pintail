use core_engine_100::{
    anchor,
    live::{Fixture, advance},
};
use std::{io::Write, time::Instant};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let n = args.get(1).map_or(1000, |s| s.parse().unwrap());
    let scenario = args.get(2).map_or(0, |s| s.parse().unwrap());
    let seed = args.get(3).map_or(41, |s| s.parse().unwrap());
    let export = args.get(4).map(std::path::PathBuf::from);
    if let Some(p) = &export {
        std::fs::create_dir_all(p).unwrap();
    }
    let mut f = Fixture::new(n, seed, scenario);
    let mut evidence = Vec::new();
    for phase in 0..8 {
        let d = f.expected();
        let snapshot = f.table.snapshot();
        let mut cases = Vec::new();
        for case in 1..=10 {
            let sql = anchor::sql(case, &d);
            let expected = anchor::expected(case, &d);
            let start = Instant::now();
            let actual = anchor::execute(&snapshot, &d, &sql);
            let ms = start.elapsed().as_secs_f64() * 1000.;
            assert_eq!(actual, expected, "engine anchor case={case} phase={phase}");
            cases.push(serde_json::json!({"case":case,"sql":sql,"ms":ms,"rows":actual.len(),"correct":true}));
            if let Some(p) = &export {
                std::fs::write(
                    p.join(format!("phase-{phase}-case-{case}.sql")),
                    format!("{sql};\n"),
                )
                .unwrap();
                std::fs::write(
                    p.join(format!("phase-{phase}-case-{case}.tsv")),
                    actual
                        .iter()
                        .map(|r| r.join("\t"))
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n",
                )
                .unwrap();
            }
        }
        if let Some(p) = export.as_ref().filter(|_| phase == 0) {
            let mut file =
                std::fs::File::create(p.join(format!("phase-{phase}-load.sql"))).unwrap();
            writeln!(file,"CREATE DATABASE IF NOT EXISTS lab; USE lab; CREATE TABLE IF NOT EXISTS facts(id BIGINT UNSIGNED PRIMARY KEY,k BIGINT UNSIGNED NOT NULL,g BIGINT UNSIGNED NOT NULL,v BIGINT NULL,padding TEXT); TRUNCATE TABLE facts;").unwrap();
            for chunk in d.rows.chunks(500) {
                let values = chunk
                    .iter()
                    .map(|r| {
                        format!(
                            "({},{},{},{},'invented')",
                            r.id,
                            r.key,
                            r.low,
                            if r.valid {
                                r.value.to_string()
                            } else {
                                "NULL".into()
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(file, "INSERT INTO facts VALUES {values};").unwrap();
            }
        }
        evidence.push(serde_json::json!({"phase":phase,"cases":cases}));
        let events = f.events(phase + 1);
        if let Some(p) = export.as_ref().filter(|_| phase < 7) {
            let mut file =
                std::fs::File::create(p.join(format!("phase-{}-load.sql", phase + 1))).unwrap();
            writeln!(file, "USE lab; START TRANSACTION;").unwrap();
            let mut versions = f.model.clone();
            for event in &events {
                if versions
                    .get(&event.row.id)
                    .is_some_and(|old| old.version >= event.version)
                {
                    continue;
                }
                versions.insert(event.row.id, *event);
                let r = event.row;
                if event.dead {
                    writeln!(file, "DELETE FROM facts WHERE id={};", r.id).unwrap();
                } else {
                    let value = if r.valid {
                        r.value.to_string()
                    } else {
                        "NULL".into()
                    };
                    writeln!(file,"INSERT INTO facts VALUES ({},{},{},{},'invented') ON DUPLICATE KEY UPDATE k={},g={},v={};",r.id,r.key,r.low,value,r.key,r.low,value).unwrap();
                }
            }
            writeln!(file, "COMMIT;").unwrap();
        }
        advance(&mut f.table, &events, phase + 1);
        f.update_model(&events);
    }
    println!(
        "{}",
        serde_json::json!({"rows":n,"scenario":scenario,"seed":seed,"phases":evidence,"correct":true})
    );
}
