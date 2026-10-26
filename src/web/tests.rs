use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::testing::{Harness, printer::PIXEL_JPEG};

fn idle() -> Value {
    json!({"gcode_state": "IDLE", "bed_temper": 20.5, "nozzle_temper": 25.0})
}

fn running() -> Value {
    json!({"gcode_state": "RUNNING", "subtask_name": "benchy.gcode", "bed_temper": 60.0})
}

#[tokio::test]
async fn status_carries_the_printers_fields() {
    let h = Harness::with_printer(json!({
        "gcode_state": "RUNNING",
        "subtask_name": "benchy.gcode",
        "layer_num": 42,
        "total_layer_num": 120,
        "mc_remaining_time": 37,
        "mc_percent": 35,
        "chamber_temper": 28.4,
        "cooling_fan_speed": 15,
        "big_fan1_speed": 0,
        "big_fan2_speed": 8,
        "ams": {"ams": [], "tray_now": "255"},
        "lights_report": [{"node": "chamber_light", "mode": "on"}],
        "hms": [{"attr": 0x03008000u32, "code": 0x00030002u32}],
    }))
    .await;
    let s = h.get("/api/status").await.json();
    assert_eq!(s["connected"], true);
    assert_eq!(s["gcodeState"], "RUNNING");
    assert_eq!(s["actionsAllowed"], false);
    assert_eq!(s["jobName"], "benchy.gcode");
    assert_eq!(
        (s["layerNum"].clone(), s["totalLayerNum"].clone()),
        (json!(42), json!(120))
    );
    assert_eq!(s["remainingMinutes"], 37);
    assert_eq!(s["progress"], 35);
    assert_eq!(s["fans"], json!({"cooling": 15, "aux": 0, "chamber": 8}));
    assert_eq!(s["ams"]["tray_now"], "255");
    assert_eq!(s["chamberLight"], true);
    assert_eq!(
        s["hms"],
        json!([{"code": "0300-8000-0003-0002", "message": "AMS filament runout"}])
    );
    assert_eq!(
        s["printActions"],
        json!({"pause": true, "resume": false, "stop": true})
    );
    assert!(s.get("chamberTemp").is_none());
    assert_eq!(s["lampOffIn"], Value::Null);
}

#[tokio::test]
async fn status_with_nothing_configured() {
    let h = Harness::offline().await;
    let s = h.get("/api/status").await.json();
    assert_eq!(s["connected"], false);
    assert_eq!(s["gcodeState"], "unknown");
    assert_eq!(s["chamberLight"], Value::Null);
    assert_eq!(s["bedOffIn"], Value::Null);
}

#[tokio::test]
async fn idle_actions_reach_the_printer_with_exact_payloads() {
    let h = Harness::with_printer(idle()).await;
    let p = h.printer();
    for (action, want) in [
        ("lower-bed", r#""param":"G90\nG1 Z200 F900\n""#),
        ("home", r#""param":"G28\n""#),
        (
            "unload",
            r#"{"print":{"command":"unload_filament","sequence_id":""#,
        ),
    ] {
        let r = h.post(&format!("/api/actions/{action}")).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::OK, format!("sent: {action}"))
        );
        p.wait_for(|req| req.contains(want)).await;
    }
}

#[tokio::test]
async fn idle_actions_are_refused_mid_print_and_without_a_printer() {
    let h = Harness::with_printer(running()).await;
    for action in [
        "lower-bed",
        "home",
        "unload",
        "set-bed-temp?temp=60",
        "set-nozzle-temp?temp=200",
        "extrude",
    ] {
        let r = h.post(&format!("/api/actions/{action}")).await;
        assert_eq!(
            (r.status, r.text()),
            (
                StatusCode::CONFLICT,
                "blocked: printer state is RUNNING\n".into()
            ),
            "{action}"
        );
    }
    let offline = Harness::offline().await;
    let r = offline.post("/api/actions/home").await;
    assert_eq!(
        (r.status, r.text()),
        (StatusCode::CONFLICT, "blocked: no printer set up\n".into())
    );
}

#[tokio::test]
async fn unknown_action() {
    let h = Harness::offline().await;
    let r = h.post("/api/actions/self-destruct").await;
    assert_eq!(
        (r.status, r.text()),
        (StatusCode::NOT_FOUND, "unknown action\n".into())
    );
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn print_actions_follow_the_print() {
    let h = Harness::with_printer(running()).await;
    let r = h.post("/api/actions/resume").await;
    assert_eq!(
        r.text(),
        "blocked: can only resume while PAUSE, printer state is RUNNING\n"
    );
    assert_eq!(h.post("/api/actions/pause").await.text(), "sent: pause");
    h.printer()
        .wait_for(|r| r.contains(r#""command":"pause""#))
        .await;
    h.report(json!({"gcode_state": "PAUSE"})).await;
    assert_eq!(
        h.post("/api/actions/pause").await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(h.post("/api/actions/resume").await.text(), "sent: resume");
    assert_eq!(h.post("/api/actions/stop").await.text(), "sent: stop");
    h.printer()
        .wait_for(|r| r.contains(r#""command":"stop""#))
        .await;
    h.report(json!({"gcode_state": "FINISH"})).await;
    assert_eq!(
        h.post("/api/actions/stop").await.text(),
        "blocked: can only stop while RUNNING or PAUSE, printer state is FINISH\n"
    );
}

#[tokio::test]
async fn temperatures_are_checked_sent_and_arm_the_shut_off() {
    let h = Harness::with_printer(idle()).await;
    for (uri, want) in [
        (
            "/api/actions/set-bed-temp?temp=abc",
            "invalid temp \"abc\"\n",
        ),
        ("/api/actions/set-bed-temp", "invalid temp \"\"\n"),
        (
            "/api/actions/set-bed-temp?temp=111",
            "temp 111 out of range 0-110\n",
        ),
        (
            "/api/actions/set-nozzle-temp?temp=-1",
            "temp -1 out of range 0-300\n",
        ),
        (
            "/api/actions/set-nozzle-temp?temp=301",
            "temp 301 out of range 0-300\n",
        ),
    ] {
        let r = h.post(uri).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::BAD_REQUEST, want.to_string()),
            "{uri}"
        );
    }
    assert_eq!(
        h.post("/api/actions/set-bed-temp?temp=60").await.text(),
        "sent: set-bed-temp 60"
    );
    h.printer()
        .wait_for(|r| r.contains(r#""param":"M140 S60\n""#))
        .await;
    assert_eq!(
        h.post("/api/actions/set-nozzle-temp?temp=220").await.text(),
        "sent: set-nozzle-temp 220"
    );
    h.printer()
        .wait_for(|r| r.contains(r#""param":"M104 S220\n""#))
        .await;
    let s = h.get("/api/status").await.json();
    assert!(
        (86390..=86400).contains(&s["bedOffIn"].as_i64().unwrap()),
        "{s}"
    );
    assert!(
        (890..=900).contains(&s["nozzleOffIn"].as_i64().unwrap()),
        "{s}"
    );
    h.post("/api/actions/set-bed-temp?temp=0").await;
    assert_eq!(h.get("/api/status").await.json()["bedOffIn"], Value::Null);
}

#[tokio::test]
async fn a_new_shut_off_window_applies_to_the_next_heater_set() {
    let h = Harness::with_printer(idle()).await;
    h.post("/api/actions/set-bed-temp?temp=60").await;
    assert_eq!(
        h.post("/api/settings/bed-off-after?value=3600")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        h.get("/api/status").await.json()["bedOffIn"]
            .as_i64()
            .unwrap()
            > 3600,
        "a running countdown keeps its window"
    );
    h.post("/api/actions/set-bed-temp?temp=65").await;
    assert!(
        h.get("/api/status").await.json()["bedOffIn"]
            .as_i64()
            .unwrap()
            <= 3600
    );
}

#[tokio::test]
async fn extrude_needs_a_hot_nozzle() {
    let h = Harness::with_printer(idle()).await;
    let r = h.post("/api/actions/extrude").await;
    assert_eq!(
        (r.status, r.text()),
        (StatusCode::CONFLICT, "blocked: nozzle below 170°C\n".into())
    );
    h.report(json!({"nozzle_temper": 215.0})).await;
    assert_eq!(h.post("/api/actions/extrude").await.text(), "sent: extrude");
    h.printer()
        .wait_for(|r| r.contains(r#""param":"M83\nG1 E20 F150\n""#))
        .await;
}

#[tokio::test]
async fn the_lamp_works_in_any_state_but_needs_the_printer() {
    let h = Harness::with_printer(running()).await;
    assert_eq!(
        h.post("/api/actions/lamp-off").await.text(),
        "sent: lamp-off"
    );
    h.printer()
        .wait_for(|r| {
            r.starts_with(r#"{"system":{"command":"ledctrl","interval_time":1000,"led_mode":"off","led_node":"chamber_light","led_off_time":500,"led_on_time":500,"loop_times":1,"sequence_id":""#)
        })
        .await;
    let offline = Harness::offline().await;
    let r = offline.post("/api/actions/lamp-on").await;
    assert_eq!(
        (r.status, r.text()),
        (StatusCode::CONFLICT, "blocked: no printer set up\n".into())
    );
}

#[tokio::test]
async fn set_filament_is_validated_normalized_and_sent() {
    let h = Harness::with_printer(idle()).await;
    let q = "ams_id=0&tray_id=1&tray_color=ff6b35&tray_type=PLA&nozzle_temp_min=190&nozzle_temp_max=230&tray_info_idx=GFA00";
    let r = h.post(&format!("/api/actions/set-filament?{q}")).await;
    assert_eq!(r.text(), "sent: set-filament ams 0 tray 1");
    h.printer()
        .wait_for(|r| r.starts_with(r#"{"print":{"ams_id":0,"command":"ams_filament_setting","nozzle_temp_max":230,"nozzle_temp_min":190,"sequence_id":""#)
            && r.ends_with(r#","tray_color":"FF6B35FF","tray_id":1,"tray_info_idx":"GFA00","tray_type":"PLA"}}"#))
        .await;

    for (change, want) in [
        ("ams_id=4", "invalid ams_id"),
        ("tray_id=x", "invalid tray_id"),
        (
            "tray_color=12345",
            "tray_color must be RRGGBB or RRGGBBAA hex",
        ),
        ("tray_color=GGGGGG", "tray_color must be hex"),
        ("tray_type=", "invalid tray_type"),
        ("nozzle_temp_min=999", "invalid nozzle_temp_min"),
        ("nozzle_temp_max=x", "invalid nozzle_temp_max"),
        (
            "nozzle_temp_min=240",
            "nozzle_temp_min above nozzle_temp_max",
        ),
    ] {
        let key = change.split('=').next().unwrap();
        let query: Vec<String> = q
            .split('&')
            .map(|kv| {
                if kv.starts_with(&format!("{key}=")) {
                    change.to_string()
                } else {
                    kv.to_string()
                }
            })
            .collect();
        let r = h
            .post(&format!("/api/actions/set-filament?{}", query.join("&")))
            .await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::BAD_REQUEST, format!("{want}\n")),
            "{change}"
        );
    }

    let busy = Harness::with_printer(running()).await;
    let r = busy.post(&format!("/api/actions/set-filament?{q}")).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_event_log_records_commands_their_acknowledgement_and_reports() {
    let h = Harness::with_printer(running()).await;
    h.post("/api/actions/pause").await;
    h.printer().wait_for(|r| r.contains("pause")).await;
    let mut acked = false;
    for _ in 0..100 {
        let events = h.get("/api/events").await.json();
        let list = events["events"].as_array().unwrap().clone();
        assert!(list.iter().any(|e| e["kind"] == "report"));
        assert!(list.iter().any(|e| e["summary"] == "pushall"));
        if list
            .iter()
            .any(|e| e["summary"] == "pause" && e.get("acked").is_some())
        {
            acked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(acked, "the broker's acknowledgement was recorded");
}

#[tokio::test]
async fn settings_are_served_and_set_in_their_units() {
    let h = Harness::offline().await;
    let s = h.get("/api/settings").await.json();
    assert_eq!(
        s,
        json!({"timelapse-storage": 4096, "bed-off-after": 86400, "nozzle-off-after": 900,
               "lamp-off-after": 28800, "activity-limit": 64, "session-length": 1209600,
               "dashboard": ""})
    );
    assert_eq!(
        h.post("/api/settings/timelapse-storage?value=2048")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.post("/api/settings/bed-off-after?value=7200")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.post("/api/settings/dashboard?text=camCard,jobCard")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let s = h.get("/api/settings").await.json();
    assert_eq!(
        (
            s["timelapse-storage"].clone(),
            s["bed-off-after"].clone(),
            s["dashboard"].clone()
        ),
        (json!(2048), json!(7200), json!("camCard,jobCard"))
    );
    assert_eq!(
        h.post("/api/settings/dashboard?text=").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.get("/api/settings").await.json()["dashboard"], "");
}

#[tokio::test]
async fn bad_setting_writes_are_refused() {
    let h = Harness::offline().await;
    for (uri, want) in [
        (
            "/api/settings/chamber-temperature?value=3600",
            "settings: unknown setting \"chamber-temperature\"",
        ),
        (
            "/api/settings/timelapse-storage?value=lots",
            "invalid value",
        ),
        ("/api/settings/timelapse-storage", "invalid value"),
        (
            "/api/settings/timelapse-storage?value=-1",
            "timelapse-storage must be between 0 MB and 1048576 MB",
        ),
        (
            "/api/settings/bed-off-after?value=1",
            "bed-off-after must be between 1h0m0s and 168h0m0s",
        ),
    ] {
        let r = h.post(uri).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::BAD_REQUEST, format!("{want}\n")),
            "{uri}"
        );
    }
    assert_eq!(
        h.get("/api/settings").await.json()["timelapse-storage"],
        4096
    );
}

#[tokio::test]
async fn adding_a_printer_connects_to_it_and_keeps_its_secret() {
    let printer =
        crate::testing::printer::FakePrinter::start("127.0.0.1", 0, 0, 0, "SERIAL1", "secret")
            .await;
    let dir = crate::testing::tempdir();
    let h = Harness::start_in(dir.clone(), crate::auth::Decision::Disabled, Some(printer)).await;
    assert_eq!(h.get("/api/printers").await.json(), json!([]));

    for (body, want) in [
        (
            r#"{"name":"Garage","ip":"127.0.0.1","serial":"SERIAL1"}"#,
            "the printer's address, serial and access code are all needed",
        ),
        (
            r#"{"name":" ","ip":"127.0.0.1","serial":"SERIAL1","accessCode":"secret"}"#,
            "give the printer a name",
        ),
        ("not json", "invalid request"),
    ] {
        let r = h.request("POST", "/api/printers", body).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::BAD_REQUEST, format!("{want}\n")),
            "{body}"
        );
    }

    let r = h
        .request(
            "POST",
            "/api/printers",
            r#"{"name":" Garage ","ip":" 127.0.0.1 ","serial":"SERIAL1","accessCode":"secret"}"#,
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_i64().unwrap();
    h.wait(|s| s.connected).await;
    let got = h.get("/api/printers").await;
    assert_eq!(
        got.json(),
        json!([{"id": id, "name": "Garage", "ip": "127.0.0.1", "serial": "SERIAL1", "accessCodeSet": true}])
    );
    assert!(!got.text().contains("secret"));

    let r = h
        .request(
            "POST",
            &format!("/api/printers/{id}"),
            r#"{"name":"Workshop","ip":"127.0.0.1","serial":"SERIAL1","accessCode":""}"#,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(h.app.printers.get(id).unwrap().cache.snapshot().connected);
    assert_eq!(h.get("/api/printers").await.json()[0]["name"], "Workshop");

    drop(h);
    let again = Harness::start_in(dir, crate::auth::Decision::Disabled, None).await;
    assert_eq!(
        again.get("/api/printers").await.json()[0]["name"],
        "Workshop"
    );
}

#[tokio::test]
async fn a_printer_that_does_not_answer_is_not_saved_and_says_why() {
    let printer =
        crate::testing::printer::FakePrinter::start("127.0.0.1", 0, 0, 0, "SERIAL1", "secret")
            .await;
    let h = Harness::start_in(
        crate::testing::tempdir(),
        crate::auth::Decision::Disabled,
        Some(printer),
    )
    .await;
    let save = |ip: &str, serial: &str, code: &str| {
        format!(r#"{{"name":"P1S","ip":"{ip}","serial":"{serial}","accessCode":"{code}"}}"#)
    };
    for (body, want) in [
        (
            save("127.0.0.1", "SERIAL1", "wrong"),
            "The printer rejected the access code.",
        ),
        (
            save("127.0.0.1", "SERIAL2", "secret"),
            "Signed in, but the printer didn't answer for serial SERIAL2. Check the serial.",
        ),
        (
            save("127.0.0.3", "SERIAL1", "secret"),
            "127.0.0.3 refused the connection. Check the address, and that LAN mode is on in the printer's network settings.",
        ),
    ] {
        let r = h.request("POST", "/api/printers", &body).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::UNPROCESSABLE_ENTITY, format!("{want}\n")),
            "{body}"
        );
    }
    assert_eq!(h.get("/api/printers").await.json(), json!([]));

    let r = h
        .request(
            "POST",
            "/api/printers",
            r#"{"name":"P1S","ip":"127.0.0.1","serial":"SERIAL1","accessCode":"wrong","skipCheck":true}"#,
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    h.wait(|s| s.problem.is_some()).await;
    let status = h.get("/api/status").await.json();
    assert_eq!(
        (
            status["connected"].clone(),
            status["configured"].clone(),
            status["problem"].clone()
        ),
        (
            json!(false),
            json!(true),
            json!("The printer rejected the access code.")
        )
    );

    let id = status["printer"].as_i64().unwrap();
    let r = h
        .request(
            "POST",
            &format!("/api/printers/{id}"),
            &save("127.0.0.1", "SERIAL1", "nope"),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn several_printers_each_get_their_own_requests_and_timelapses() {
    let one =
        crate::testing::printer::FakePrinter::start("127.0.0.1", 0, 0, 0, "SERIAL1", "secret")
            .await;
    let two = crate::testing::printer::FakePrinter::start(
        "127.0.0.2",
        one.mqtt_port,
        one.camera_port,
        one.ftp_port,
        "SERIAL2",
        "secret2",
    )
    .await;
    one.set_state(idle());
    two.set_state(json!({"gcode_state": "RUNNING", "mc_percent": 40}));
    let h = Harness::start_in(
        crate::testing::tempdir(),
        crate::auth::Decision::Disabled,
        Some(one.clone()),
    )
    .await;
    let add = |name: &str, ip: &str, serial: &str, code: &str| {
        format!(r#"{{"name":"{name}","ip":"{ip}","serial":"{serial}","accessCode":"{code}"}}"#)
    };
    let id1 = h
        .request(
            "POST",
            "/api/printers",
            &add("Garage", "127.0.0.1", "SERIAL1", "secret"),
        )
        .await
        .json()["id"]
        .as_i64()
        .unwrap();
    let r = h
        .request(
            "POST",
            "/api/printers",
            &add("garage", "127.0.0.2", "SERIAL2", "secret2"),
        )
        .await;
    assert_eq!(
        (r.status, r.text()),
        (
            StatusCode::BAD_REQUEST,
            "there is already a printer called garage\n".into()
        )
    );
    let id2 = h
        .request(
            "POST",
            "/api/printers",
            &add("Office", "127.0.0.2", "SERIAL2", "secret2"),
        )
        .await
        .json()["id"]
        .as_i64()
        .unwrap();
    h.wait_on(id1, |s| s.connected && !s.fields.is_empty())
        .await;
    h.wait_on(id2, |s| s.connected && !s.fields.is_empty())
        .await;

    assert_eq!(h.get("/api/status").await.json()["printer"], id1);
    let s2 = h.get(&format!("/api/status?printer={id2}")).await.json();
    assert_eq!(
        (s2["printer"].clone(), s2["gcodeState"].clone()),
        (json!(id2), json!("RUNNING"))
    );
    assert_eq!(
        h.get("/api/status?printer=999").await.text(),
        "no such printer\n"
    );
    assert_eq!(
        h.post(&format!("/api/actions/pause?printer={id2}"))
            .await
            .text(),
        "sent: pause"
    );
    two.wait_for(|r| r.contains("\"pause\"")).await;
    assert!(!one.requests().iter().any(|r| r.contains("\"pause\"")));
    assert_eq!(
        h.post(&format!("/api/actions/home?printer={id1}"))
            .await
            .text(),
        "sent: home"
    );
    one.wait_for(|r| r.contains("G28")).await;
    assert!(!two.requests().iter().any(|r| r.contains("G28")));

    let video = "video_2026-07-23_08-45-39.avi";
    two.put_file(&format!("/timelapse/{video}"), Vec::new());
    two.put_file(
        "/timelapse/thumbnail/video_2026-07-23_08-45-39.jpg",
        vec![2],
    );
    for id in [id1, id2] {
        h.app
            .printers
            .get(id)
            .unwrap()
            .library
            .refresh()
            .await
            .unwrap();
    }
    let thumbnail =
        |printer: i64| format!("/api/timelapses/thumbnail?name={video}&printer={printer}");
    assert_eq!(h.get(&thumbnail(id2)).await.body, vec![2]);
    assert_eq!(h.get(&thumbnail(id1)).await.status, StatusCode::NOT_FOUND);

    assert_eq!(h.app.printers.label(id2), "Office: ");

    assert_eq!(
        h.request("DELETE", &format!("/api/printers/{id2}"), "")
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert!(h.app.printers.get(id2).is_none() && !h.app.printers.stored(id2));
    assert_eq!(
        h.get("/api/printers")
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        h.request("DELETE", &format!("/api/printers/{id2}"), "")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.app.printers.label(id1), "");
}

#[tokio::test]
async fn timelapses_on_the_sd_card_are_played_frame_by_frame() {
    let h = Harness::with_printer(idle()).await;
    let video = "video_2026-07-23_08-45-39.avi";
    let other = [PIXEL_JPEG, &[0]].concat();
    h.printer().put_file(
        &format!("/timelapse/{video}"),
        crate::testing::printer::avi(&[PIXEL_JPEG, &other], 41_666),
    );
    h.printer().put_file(
        "/timelapse/thumbnail/video_2026-07-23_08-45-39.jpg",
        vec![7],
    );
    let frame = |i: &str| format!("/api/timelapses/frame?name={video}&i={i}");
    assert_eq!(
        h.get(&frame("0")).await.status,
        StatusCode::NOT_FOUND,
        "not listed yet"
    );
    h.app
        .printers
        .first()
        .unwrap()
        .library
        .refresh()
        .await
        .unwrap();

    let r = h.get(&frame("1")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "image/jpeg");
    assert_eq!(r.headers["x-frame-count"], "2");
    assert_eq!(r.headers["x-frame-interval"], "41666");
    assert_eq!(r.body, other);
    assert_eq!(h.get(&frame("0")).await.body, PIXEL_JPEG);
    assert_eq!(h.get(&frame("2")).await.text(), "no such timelapse\n");
    assert_eq!(h.get(&frame("x")).await.text(), "name and i are required\n");
    assert_eq!(
        h.get("/api/timelapses/frame?name=../cache/a.gcode&i=0")
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let r = h
        .get(&format!("/api/timelapses/thumbnail?name={video}"))
        .await;
    assert_eq!((r.status, r.body), (StatusCode::OK, vec![7]));
    assert_eq!(r.headers["cache-control"], "private, max-age=86400");
}

#[tokio::test]
async fn a_timelapse_that_is_not_an_avi_is_reported() {
    let h = Harness::with_printer(idle()).await;
    let video = "video_2026-07-23_08-45-39.avi";
    h.printer()
        .put_file(&format!("/timelapse/{video}"), b"not a video".to_vec());
    h.app
        .printers
        .first()
        .unwrap()
        .library
        .refresh()
        .await
        .unwrap();
    let r = h
        .get(&format!("/api/timelapses/frame?name={video}&i=0"))
        .await;
    assert_eq!(
        (r.status, r.text()),
        (StatusCode::BAD_GATEWAY, "not an AVI file\n".into())
    );
}

#[tokio::test]
async fn the_live_camera_keeps_its_latest_frame() {
    let h = Harness::with_printer(idle()).await;
    let hub = h.app.printers.first().unwrap().hub.clone();
    for _ in 0..100 {
        if hub.latest().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(hub.latest().expect("a frame").1, PIXEL_JPEG);
}

#[tokio::test]
async fn the_page_and_its_files_are_served() {
    let h = Harness::offline().await;
    let page = h.get("/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(page.headers["cache-control"], "no-cache");
    assert!(page.text().contains("<script>"));
    for (path, kind) in [
        ("/sw.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
        ("/manifest.webmanifest", "application/manifest+json"),
        ("/icon-192.png", "image/png"),
        ("/icon-512.png", "image/png"),
    ] {
        let r = h.get(path).await;
        assert_eq!(
            (r.status, r.headers["content-type"].to_str().unwrap()),
            (StatusCode::OK, kind),
            "{path}"
        );
    }
    let r = h.get("/index.html").await;
    assert_eq!(
        (r.status, r.headers["location"].to_str().unwrap()),
        (StatusCode::MOVED_PERMANENTLY, "./")
    );
    assert_eq!(h.get("/nope.js").await.status, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/healthz").await.status, StatusCode::OK);
    assert_eq!(h.request("HEAD", "/", "").await.status, StatusCode::OK);
    assert_eq!(
        h.post("/api/status").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn cross_origin_posts_are_refused() {
    let h = Harness::offline().await;
    let post = |headers: &[(&str, &str)]| {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/settings/timelapse-storage?value=512");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.body(axum::body::Body::empty()).unwrap()
    };
    let cases: &[(&[(&str, &str)], StatusCode)] = &[
        (&[], StatusCode::NO_CONTENT),
        (&[("sec-fetch-site", "same-origin")], StatusCode::NO_CONTENT),
        (&[("sec-fetch-site", "cross-site")], StatusCode::FORBIDDEN),
        (&[("sec-fetch-site", "same-site")], StatusCode::FORBIDDEN),
        (
            &[("host", "printer.lan"), ("origin", "http://printer.lan")],
            StatusCode::NO_CONTENT,
        ),
        (
            &[("host", "printer.lan"), ("origin", "http://evil.example")],
            StatusCode::FORBIDDEN,
        ),
    ];
    for (headers, want) in cases {
        assert_eq!(h.send(post(headers)).await.status, *want, "{headers:?}");
    }
    let get = axum::http::Request::builder()
        .uri("/api/status")
        .header("sec-fetch-site", "cross-site")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(h.send(get).await.status, StatusCode::OK);
}

#[tokio::test]
async fn push_subscriptions_and_preferences() {
    let h = Harness::offline().await;
    let key = h.get("/api/push/key").await.json();
    assert_eq!(key["subscribed"], 0);
    assert_eq!(key["key"].as_str().unwrap().len(), 87);

    let ua =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    let sub = json!({"endpoint": "https://push.example.com/abc", "expirationTime": null, "keys": {"p256dh": ua, "auth": "BTBZMqHH6r4Tts7J_aSIgg"}});
    for (body, want) in [
        ("nope".to_string(), "invalid subscription"),
        (
            json!({"endpoint": "https://x/", "keys": {"p256dh": "%%%", "auth": "AA"}}).to_string(),
            "subscription key is not base64url",
        ),
        (
            json!({"endpoint": "https://x/", "keys": {"p256dh": ua, "auth": "AA=="}}).to_string(),
            "auth secret is not base64url",
        ),
        (
            json!({"endpoint": "https://x/", "keys": {"p256dh": "AAAA", "auth": "AA"}}).to_string(),
            "push: subscription key is 3 bytes, want 65",
        ),
    ] {
        let r = h.request("POST", "/api/push/subscribe", &body).await;
        assert_eq!(
            (r.status, r.text()),
            (StatusCode::BAD_REQUEST, format!("{want}\n")),
            "{body}"
        );
    }
    assert_eq!(
        h.request("POST", "/api/push/subscribe", &sub.to_string())
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.get("/api/push/key").await.json()["subscribed"], 1);

    async fn prefs(h: &Harness) -> Value {
        h.get("/api/push/preferences?endpoint=https://push.example.com/abc")
            .await
            .json()
    }
    let all = json!([
        "print-started",
        "print-finished",
        "print-ended",
        "printer-error",
        "heater-off"
    ]);
    assert_eq!(
        prefs(&h).await,
        json!({"available": all, "kinds": all, "bedInterval": 0})
    );

    let set = |kinds: Value, interval: i64| {
        json!({"endpoint": "https://push.example.com/abc", "kinds": kinds, "bedInterval": interval})
            .to_string()
    };
    assert_eq!(
        h.request(
            "POST",
            "/api/push/preferences",
            &set(json!(["printer-error"]), 3600)
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(prefs(&h).await["kinds"], json!(["printer-error"]));
    assert_eq!(prefs(&h).await["bedInterval"], 3600);
    assert_eq!(
        h.request("POST", "/api/push/preferences", &set(json!([]), 0))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(prefs(&h).await["kinds"], json!([]));

    let r = h
        .request("POST", "/api/push/preferences", &set(json!(["nope"]), 0))
        .await;
    assert_eq!(r.text(), "push: unknown notification \"nope\"\n");
    assert_eq!(
        h.request("POST", "/api/push/preferences", "{}")
            .await
            .text(),
        "invalid request\n"
    );
    assert_eq!(h.get("/api/push/preferences").await.text(), "no endpoint\n");
    assert_eq!(
        h.get("/api/push/preferences?endpoint=https://x/")
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        h.request("POST", "/api/push/unsubscribe", "{}")
            .await
            .text(),
        "invalid request\n"
    );
    assert_eq!(
        h.request(
            "POST",
            "/api/push/unsubscribe",
            r#"{"endpoint":"https://push.example.com/abc"}"#
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.get("/api/push/key").await.json()["subscribed"], 0);
}

#[tokio::test]
async fn the_test_notification_reaches_a_subscribed_device() {
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<(String, usize)>>> = Arc::default();
    let record = seen.clone();
    let service = axum::Router::new().route(
        "/send",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let record = record.clone();
                async move {
                    let auth = headers["authorization"].to_str().unwrap().to_string();
                    assert_eq!(headers["content-encoding"], "aes128gcm");
                    assert_eq!(headers["ttl"], "14400");
                    record.lock().unwrap().push((auth, body.len()));
                    StatusCode::CREATED
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, service).await });

    let h = Harness::offline().await;
    let sub = json!({"endpoint": format!("http://{addr}/send"), "keys": {
        "p256dh": "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        "auth": "BTBZMqHH6r4Tts7J_aSIgg"}});
    h.request("POST", "/api/push/subscribe", &sub.to_string())
        .await;
    let r = h.post("/api/push/test").await;
    assert_eq!(r.json(), json!({"delivered": 1}));
    {
        let seen = seen.lock().unwrap();
        assert!(seen[0].0.starts_with("vapid t="));
        assert!(seen[0].1 > 86);
    }
    let events = h.get("/api/events").await.json();
    assert_eq!(events["events"][0]["summary"], "BambuSpa → 1 of 1 devices");
}

mod live {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    async fn connect(
        addr: std::net::SocketAddr,
        origin: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Error,
    > {
        let mut req = format!("ws://{addr}/api/live")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert("origin", origin.parse().unwrap());
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    async fn next_of(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        want: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let msg = tokio::time::timeout_at(deadline, ws.next())
                .await
                .expect("timed out")
                .unwrap()
                .unwrap();
            if let Message::Text(text) = msg {
                let v: Value = serde_json::from_str(&text).unwrap();
                if want(&v) {
                    return v;
                }
            }
        }
    }

    #[tokio::test]
    async fn sends_a_snapshot_then_changes() {
        let h = Harness::with_printer(idle()).await;
        let addr = h.serve().await;
        let mut ws = connect(addr, &format!("http://{addr}")).await.unwrap();
        let status = next_of(&mut ws, |v| v["type"] == "status").await;
        assert_eq!(status["gcodeState"], "IDLE");
        assert_eq!(status["connected"], true);
        let timelapses = next_of(&mut ws, |v| v["type"] == "timelapses").await;
        assert_eq!(timelapses["timelapses"], json!([]));

        h.printer()
            .report(json!({"gcode_state": "RUNNING", "subtask_name": "benchy"}));
        let status = next_of(&mut ws, |v| {
            v["type"] == "status" && v["gcodeState"] == "RUNNING"
        })
        .await;
        assert_eq!(status["jobName"], "benchy");

        h.printer()
            .put_file("/timelapse/video_2026-07-23_08-45-39.avi", vec![0; 3]);
        h.app
            .printers
            .first()
            .unwrap()
            .library
            .refresh()
            .await
            .unwrap();
        let timelapses = next_of(&mut ws, |v| {
            v["type"] == "timelapses" && !v["timelapses"].as_array().unwrap().is_empty()
        })
        .await;
        assert_eq!(
            timelapses["timelapses"],
            json!([{"name": "video_2026-07-23_08-45-39.avi", "bytes": 3}])
        );
    }

    #[tokio::test]
    async fn frames_come_only_while_following() {
        let h = Harness::with_printer(idle()).await;
        let addr = h.serve().await;
        let mut ws = connect(addr, &format!("http://{addr}")).await.unwrap();
        let until = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(until, ws.next()).await {
            assert!(
                !matches!(msg, Message::Binary(_)),
                "a frame arrived before following"
            );
        }
        ws.send(Message::Text(r#"{"type":"follow","on":true}"#.into()))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let msg = tokio::time::timeout_at(deadline, ws.next())
                .await
                .expect("no frame")
                .unwrap()
                .unwrap();
            if let Message::Binary(data) = msg {
                let ts = i64::from_be_bytes(data[..8].try_into().unwrap());
                assert!(ts > 1_700_000_000);
                assert_eq!(&data[8..], PIXEL_JPEG);
                break;
            }
        }
    }

    #[tokio::test]
    async fn another_origin_is_refused() {
        let h = Harness::offline().await;
        let addr = h.serve().await;
        let err = connect(addr, "http://evil.example").await.unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
    }
}
