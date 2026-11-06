//! The unit's weekly schedule (its schedule bucket) as Home Assistant reads
//! and writes it: per day, the times the target changes and the temperature
//! it changes to.
//!
//! The bucket keeps each day as an object of entries keyed "0", "1", ... in
//! time order, days "0" (Monday) to "6" (Sunday). Entry "0" of a day is its
//! continuation: midnight, at the previous day's last setpoint. A write
//! replaces "days" whole, so a command changing some days rebuilds the week:
//! days it names get new setpoints, the others keep theirs, and every
//! continuation follows the day before it.

use serde_json::Value;

use crate::bridge::shown;
use crate::fields;
use crate::json::{num, number, sprint, Fields};

pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Marks the setpoints a command writes as the owner's, as nlclient marks
/// the Nest app's.
const TOUCHED_BY_LOCAL: i64 = 4;

/// Setpoint temperatures, °C.
const SETPOINT: (f64, f64) = (4.5, 35.0);

fn entries(day: Option<&Value>) -> Vec<Fields> {
    let mut v: Vec<(i64, Fields)> = day.and_then(Value::as_object).map_or(Vec::new(), |d| {
        d.iter().filter_map(|(k, e)| Some((k.parse().ok()?, e.as_object()?.clone()))).collect()
    });
    v.sort_by_key(|(k, _)| *k);
    v.into_iter().map(|(_, e)| e).collect()
}

fn setpoints(day: Option<&Value>) -> Vec<Fields> {
    let mut v: Vec<Fields> = entries(day).into_iter().filter(|e| sprint(e.get("entry_type")) != "continuation").collect();
    v.sort_by(|a, b| number(a.get("time")).partial_cmp(&number(b.get("time"))).unwrap_or(std::cmp::Ordering::Equal));
    v
}

fn hhmm(seconds: f64) -> String {
    let m = (seconds / 60.0).round() as i64;
    format!("{:02}:{:02}", m / 60 % 24, m % 60)
}

/// The schedule as Home Assistant reads it: its mode and, by day, the
/// setpoints in time order as {"time": "HH:MM", "temp": °C}, temperatures
/// as the unit's screen shows them.
pub fn simple(bucket: &Fields, scale: &str) -> Value {
    let days = bucket.get("days");
    let mut out = Fields::new();
    for (i, name) in DAYS.iter().enumerate() {
        let list: Vec<Value> = setpoints(days.and_then(|d| d.get(i.to_string())))
            .iter()
            .filter_map(|e| {
                let t = number(e.get("time"))?;
                Some(Value::Object(fields!{"time" => hhmm(t), "temp" => shown(e.get("temp"), scale)}))
            })
            .collect();
        out.insert((*name).into(), Value::Array(list));
    }
    Value::Object(fields!{
        "mode" => sprint(bucket.get("schedule_mode")).to_lowercase(),
        "days" => Value::Object(out),
    })
}

/// "HH:MM" as seconds after midnight.
fn seconds(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m) && s.len() == 5).then_some(h * 3600 + m * 60)
}

/// One day's new setpoints: its index from Monday, and (seconds, °C) in
/// time order.
type DayChange = (usize, Vec<(i64, f64)>);

/// The days a schedule command names, as new setpoints: {"mon": [{"time":
/// "06:00", "temp": 20.5}, ...], ...}, any subset of the days.
fn parse(arg: &str) -> Result<Vec<DayChange>, String> {
    let bad = |e: &str| Err(format!("expected {{\"mon\": [{{\"time\": \"06:00\", \"temp\": 20.5}}], ...}}: {e}"));
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(arg) else {
        return bad("not a JSON object");
    };
    let mut out = Vec::new();
    for (name, list) in &o {
        let Some(day) = DAYS.iter().position(|d| d == name) else {
            return bad(&format!("unknown day {name}"));
        };
        let Some(list) = list.as_array() else {
            return bad(&format!("{name} is not a list"));
        };
        let mut points = Vec::new();
        for p in list {
            let (Some(t), Some(c)) = (p.get("time").and_then(Value::as_str).and_then(seconds), number(p.get("temp"))) else {
                return bad(&format!("{name}: each setpoint needs a time HH:MM and a temp"));
            };
            if !(SETPOINT.0..=SETPOINT.1).contains(&c) {
                return bad(&format!("{name}: temp {c} outside {}-{} °C", SETPOINT.0, SETPOINT.1));
            }
            if points.iter().any(|(pt, _)| *pt == t) {
                return bad(&format!("{name}: two setpoints at {}", hhmm(t as f64)));
            }
            points.push((t, (c * 2.0).round() / 2.0));
        }
        points.sort_by_key(|(t, _)| *t);
        out.push((day, points));
    }
    Ok(out)
}

/// The bucket fields a schedule command writes: the whole week, with the
/// days it names replaced.
pub fn write(bucket: &Fields, arg: &str, now: i64) -> Result<Fields, String> {
    let changes = parse(arg)?;
    let days = bucket.get("days");
    let kind = match sprint(bucket.get("schedule_mode")).as_str() {
        "" | "<nil>" => "HEAT".to_string(),
        m => m.to_string(),
    };
    let entry = |time: i64, temp: f64, entry_type: &str| fields!{
        "entry_type" => entry_type,
        "temp" => num(temp),
        "time" => num(time as f64),
        "touched_at" => num(now as f64),
        "touched_by" => num(TOUCHED_BY_LOCAL as f64),
        "touched_tzo" => 0,
        "type" => kind.clone(),
    };

    // Each day's setpoints: the command's, or the ones it has.
    let mut week: Vec<Vec<Fields>> = (0..7).map(|d| setpoints(days.and_then(|x| x.get(d.to_string())))).collect();
    for (d, points) in &changes {
        week[*d] = points.iter().map(|(t, c)| entry(*t, *c, "setpoint")).collect();
    }

    // Midnight carries on from the last setpoint before it, however many
    // days back; a week with none keeps the continuations it has.
    let last = |d: usize| (1..=7).find_map(|back| number(week[(d + 7 - back) % 7].last()?.get("temp")));
    let mut out = Fields::new();
    for (d, points) in week.iter().enumerate() {
        let old = entries(days.and_then(|x| x.get(d.to_string())));
        let old_cont = old.iter().find(|e| sprint(e.get("entry_type")) == "continuation");
        let cont = match (last(d), old_cont) {
            (Some(t), Some(c)) if number(c.get("temp")) == Some(t) => Some(c.clone()),
            (Some(t), _) => Some(entry(0, t, "continuation")),
            (None, c) => c.cloned(),
        };
        let mut day = Fields::new();
        for (i, e) in cont.into_iter().chain(points.iter().cloned()).enumerate() {
            day.insert(i.to_string(), Value::Object(e));
        }
        out.insert(d.to_string(), Value::Object(day));
    }
    Ok(fields!{"days" => Value::Object(out)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::canonical;
    use serde_json::json;

    fn obj(v: Value) -> Fields {
        match canonical(v) {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    fn sp(time: i64, temp: f64) -> Value {
        json!({"entry_type": "setpoint", "temp": temp, "time": time, "touched_at": 1, "touched_by": 5, "touched_tzo": 0, "type": "HEAT"})
    }

    fn cont(temp: f64) -> Value {
        json!({"entry_type": "continuation", "temp": temp, "time": 0, "touched_at": 1, "touched_by": 1, "touched_tzo": 0, "type": "HEAT"})
    }

    /// Every day: 06:00 20.5, 22:00 18.
    fn bucket() -> Fields {
        let day = json!({"0": cont(18.0), "1": sp(21600, 20.5), "2": sp(79200, 18.0)});
        let days: serde_json::Map<String, Value> = (0..7).map(|d| (d.to_string(), day.clone())).collect();
        obj(json!({"days": days, "schedule_mode": "HEAT", "ver": 2}))
    }

    #[test]
    fn reads_setpoints_by_day_without_continuations() {
        let mut b = bucket();
        b["days"]["4"]["2"] = canonical(sp(79200, 17.89737));
        let s = simple(&b, "C");
        assert_eq!(s["mode"], json!("heat"));
        assert_eq!(s["days"]["mon"], json!([{"time": "06:00", "temp": 20.5}, {"time": "22:00", "temp": 18}]));
        assert_eq!(s["days"]["fri"][1], json!({"time": "22:00", "temp": 18}), "shown as the screen rounds it");
        assert_eq!(simple(&Fields::new(), "C")["days"]["sun"], json!([]));
    }

    #[test]
    fn writes_the_named_days_and_keeps_the_rest() {
        let b = bucket();
        let f = write(&b, r#"{"fri": [{"time": "22:30", "temp": 17}, {"time": "07:00", "temp": 21.2}]}"#, 99).unwrap();
        let days = &f["days"];
        let fri = &days["4"];
        assert_eq!(fri.as_object().unwrap().len(), 3);
        assert_eq!((&fri["1"]["time"], &fri["1"]["temp"], &fri["1"]["touched_by"]), (&json!(25200), &json!(21), &json!(4)));
        assert_eq!((&fri["2"]["time"], &fri["2"]["temp"]), (&json!(81000), &json!(17)));
        assert_eq!(fri["0"], canonical(cont(18.0)), "an unchanged continuation stays as it was");
        // Saturday carries on from Friday's new last setpoint.
        assert_eq!((&days["5"]["0"]["entry_type"], &days["5"]["0"]["temp"]), (&json!("continuation"), &json!(17)));
        assert_eq!(days["5"]["1"], canonical(sp(21600, 20.5)), "untouched days keep their entries");
        assert_eq!(days["0"], b["days"]["0"]);
    }

    #[test]
    fn an_empty_day_carries_on_from_the_day_before() {
        let f = write(&bucket(), r#"{"sat": []}"#, 1).unwrap();
        assert_eq!(f["days"]["5"].as_object().unwrap().len(), 1, "only its continuation");
        assert_eq!(f["days"]["6"]["0"]["temp"], json!(18), "Sunday from Friday's 18");
    }

    #[test]
    fn refuses_what_the_unit_cannot_take() {
        for arg in ["[]", "{", r#"{"funday": []}"#, r#"{"mon": {}}"#, r#"{"mon": [{"time": "6:00", "temp": 20}]}"#,
                    r#"{"mon": [{"time": "24:00", "temp": 20}]}"#, r#"{"mon": [{"time": "06:00", "temp": 40}]}"#,
                    r#"{"mon": [{"time": "06:00"}]}"#,
                    r#"{"mon": [{"time": "06:00", "temp": 20}, {"time": "06:00", "temp": 21}]}"#] {
            assert!(write(&bucket(), arg, 1).is_err(), "{arg} accepted");
        }
    }
}
