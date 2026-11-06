use super::*;
use crate::prompt::tenant_context_block;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0192_f6e0_7a3c_7d4e_8f00_0000_0000_0000 | n)
}

fn taipei() -> UserSettings {
    UserSettings {
        time_zone: Some("Asia/Taipei".to_owned()),
        ai_language: Some(AiLanguage::ZhTw),
    }
}

fn point(key: &str, value_type: ValueType, description: Option<&str>) -> PointEntry {
    PointEntry {
        key: key.to_owned(),
        value_type,
        description: description.map(str::to_owned),
    }
}

fn device(n: u128, name: &str, model: u128) -> DeviceEntry {
    DeviceEntry {
        id: id(n),
        name: name.to_owned(),
        model_id: id(model),
    }
}

/// Two models, three devices, given out of order.
fn small() -> Snapshot {
    Snapshot {
        devices: vec![
            device(12, "boiler-room", 2),
            device(11, "lab-sensor", 1),
            device(10, "annex-sensor", 1),
        ],
        models: vec![
            ModelEntry {
                id: id(2),
                name: "power-meter".to_owned(),
                point_count: 1,
                points: Some(vec![point("energy", ValueType::Float, None)]),
            },
            ModelEntry {
                id: id(1),
                name: "temp-humidity".to_owned(),
                point_count: 2,
                points: Some(vec![
                    point(
                        "temperature",
                        ValueType::Float,
                        Some("\u{6EAB}\u{5EA6}\u{FF08}\u{00B0}C\u{FF09}"),
                    ),
                    point("humidity", ValueType::Float, Some("Relative humidity, %")),
                ]),
            },
        ],
    }
}

#[test]
fn the_context_names_the_settings_devices_and_data_points_in_a_fixed_order() {
    let text = render(&taipei(), &Devices::Read(small()));
    let expected = format!(
        "{PREFACE}\n\
         \n\
         The user's settings:\n\
         - Time zone: Asia/Taipei. Use it for local days, weeks and months and for the times you \
         show, unless the user asks for another zone.\n\
         - Answer language: Traditional Chinese (zh-TW), whatever language a question is in.\n\
         \n\
         Devices and data points: 3 devices of 2 models. In telemetry, device_id is the device \
         ID and point is the data point key.\n\
         Model power-meter (1 data points):\n\
         \x20 Devices [name, device ID]:\n\
         \x20 - boiler-room, {}\n\
         \x20 Data points [key, value type, description]:\n\
         \x20 - energy, float\n\
         Model temp-humidity (2 data points):\n\
         \x20 Devices [name, device ID]:\n\
         \x20 - annex-sensor, {}\n\
         \x20 - lab-sensor, {}\n\
         \x20 Data points [key, value type, description]:\n\
         \x20 - humidity, float, Relative humidity, %\n\
         \x20 - temperature, float, \u{6EAB}\u{5EA6}\u{FF08}\u{00B0}C\u{FF09}",
        id(12),
        id(10),
        id(11)
    );
    assert_eq!(text, expected);
}

#[test]
fn the_same_data_in_any_order_gives_the_same_bytes() {
    let first = render(&taipei(), &Devices::Read(small()));
    let mut shuffled = small();
    shuffled.devices.reverse();
    shuffled.models.reverse();
    for model in &mut shuffled.models {
        if let Some(points) = &mut model.points {
            points.reverse();
        }
    }
    assert_eq!(render(&taipei(), &Devices::Read(shuffled)), first);
    assert_eq!(render(&taipei(), &Devices::Read(small())), first);
}

#[test]
fn settings_that_are_not_known_say_so() {
    let text = render(&UserSettings::default(), &Devices::Read(small()));
    assert!(text.contains("- Time zone: not known. Use UTC unless the user names a zone.\n"));
    assert!(text.contains("- Answer language: not set; the language of each question.\n"));
    let auto = UserSettings {
        time_zone: Some("UTC".to_owned()),
        ai_language: Some(AiLanguage::Auto),
    };
    let text = render(&auto, &Devices::Read(small()));
    assert!(text.contains("- Time zone: UTC. Use it for local days"));
    assert!(text.contains("- Answer language: the language of each question.\n"));
}

#[test]
fn without_devices_the_context_has_the_settings_and_says_why() {
    let unavailable = render(&taipei(), &Devices::Unavailable);
    assert!(unavailable.contains("- Time zone: Asia/Taipei."));
    assert!(unavailable.ends_with(DEVICES_UNAVAILABLE));
    let none = render(&taipei(), &Devices::Read(Snapshot::default()));
    assert!(none.contains("- Time zone: Asia/Taipei."));
    assert!(none.ends_with(NO_DEVICES));
}

/// A tenant with `models` models of `points` data points (each with a long description) and
/// `devices` devices per model.
fn large(models: u128, devices: u128, points: usize) -> Snapshot {
    let description = "\u{4F9B}\u{6C34}\u{6EAB}\u{5EA6}".repeat(40);
    let mut snapshot = Snapshot::default();
    for model in 0..models {
        snapshot.models.push(ModelEntry {
            id: id(1000 + model),
            name: format!("model-{model:03}"),
            point_count: u64::try_from(points).expect("fits"),
            points: Some(
                (0..points)
                    .map(|n| point(&format!("point-{n:04}"), ValueType::Int, Some(&description)))
                    .collect(),
            ),
        });
        for n in 0..devices {
            snapshot.devices.push(device(
                model * 1000 + n,
                &format!("device-{model:03}-{n:03}"),
                1000 + model,
            ));
        }
    }
    snapshot
}

fn block(text: &str) -> usize {
    tenant_context_block(text).len()
}

#[test]
fn descriptions_go_first_when_the_whole_list_is_too_long() {
    // 2 models of 30 points with long descriptions: too long with them, fits without.
    let snapshot = large(2, 3, 30);
    let text = render(&taipei(), &Devices::Read(snapshot));
    assert!(block(&text) <= MAX_CONTEXT_BYTES, "{}", block(&text));
    assert!(text.contains(DESCRIPTIONS_LEFT_OUT));
    assert!(!text.contains('\u{4F9B}'), "no description is left");
    assert!(text.contains("  - point-0029, int\n") || text.ends_with("  - point-0029, int"));
    assert!(
        !text.contains("Not listed here"),
        "everything else is listed"
    );
}

#[test]
fn a_list_still_too_long_stops_at_a_whole_line_and_says_what_is_missing() {
    let snapshot = large(6, 20, 60);
    let first = render(&taipei(), &Devices::Read(snapshot.clone()));
    assert_eq!(
        render(&taipei(), &Devices::Read(snapshot)),
        first,
        "deterministic"
    );
    let size = block(&first);
    assert!(size <= MAX_CONTEXT_BYTES, "{size}");
    assert!(size > MAX_CONTEXT_BYTES - 200, "it fills the room: {size}");
    // The settings always stay.
    assert!(first.contains("- Time zone: Asia/Taipei."));
    // Models in order: the first is listed whole, then the list stops.
    assert!(first.contains("Model model-000 (60 data points):"));
    assert!(first.contains("  - device-000-019, "));
    assert!(first.contains("  - point-0059, int\n"));
    let (before, note) = first.rsplit_once('\n').expect("lines");
    assert!(
        !before.ends_with(':'),
        "no heading is left without a line under it"
    );
    let listed_devices = first.matches("  - device-").count();
    let listed_points = first.matches("  - point-").count();
    assert_eq!(
        note,
        format!(
            "Not listed here: {} more devices and {} more data points. Find them with the tools: \
             telemetry_sql (for example SELECT DISTINCT device_id, point FROM \
             tenant_telemetry_1h) and device_models.",
            120 - listed_devices,
            360 - listed_points
        )
    );
}

#[test]
fn models_whose_points_were_not_read_are_named_with_their_count() {
    let mut snapshot = small();
    snapshot.models[0].points = None;
    snapshot.models[0].point_count = 12;
    let text = render(&taipei(), &Devices::Read(snapshot));
    assert!(text.contains("Model power-meter (12 data points, not listed here):\n"));
    assert!(text.contains("  - boiler-room, "));
    assert!(text.ends_with(
        "Not listed here: 0 more devices and 12 more data points. Find them with the tools: \
         telemetry_sql (for example SELECT DISTINCT device_id, point FROM tenant_telemetry_1h) \
         and device_models."
    ));
}

#[test]
fn members_text_stays_on_its_line_without_hidden_characters() {
    assert_eq!(clean("a\nb\tc\r\n  d", 64), "a b c d");
    assert_eq!(
        clean("left\u{202E}right\u{200B}\u{2066}x\u{2028}y", 64),
        "leftrightx y"
    );
    assert_eq!(clean("  padded  ", 64), "padded");
    let long = "\u{6EAB}".repeat(200);
    let cut = clean(&long, MAX_DESCRIPTION_CHARS);
    assert_eq!(cut.chars().count(), MAX_DESCRIPTION_CHARS);
    assert!(cut.ends_with('…'));
    // A description that is only hidden characters is left out.
    let mut snapshot = small();
    if let Some(points) = &mut snapshot.models[0].points {
        points[0].description = Some("\u{200B}\n".to_owned());
    }
    let text = render(&taipei(), &Devices::Read(snapshot));
    assert!(text.contains("  - energy, float\n"));
}

#[test]
fn the_value_type_is_the_one_telemetry_stores() {
    for (data_type, scaled, expected) in [
        ("bool", false, ValueType::Bool),
        ("string", false, ValueType::String),
        ("f32", false, ValueType::Float),
        ("f64", true, ValueType::Float),
        ("u16", false, ValueType::Int),
        ("i64", false, ValueType::Int),
        ("u32", true, ValueType::Float),
        ("i16", true, ValueType::Float),
        ("f128", false, ValueType::Unknown),
    ] {
        assert_eq!(
            ValueType::of(data_type, scaled),
            expected,
            "{data_type} {scaled}"
        );
    }
}

#[test]
fn time_zone_names_have_the_iana_form() {
    for good in [
        "UTC",
        "Asia/Taipei",
        "America/Argentina/Buenos_Aires",
        "Etc/GMT+8",
    ] {
        assert!(is_time_zone(good), "{good}");
    }
    let long = format!("Area/{}", "a".repeat(60));
    for bad in [
        "",
        "Asia/",
        "/Asia",
        "Asia/Tai pei",
        "1Asia/Taipei",
        "Asia/<b>",
        &long,
    ] {
        assert!(!is_time_zone(bad), "{bad}");
    }
}
