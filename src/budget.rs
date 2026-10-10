//! Persistent conservative reservations.
//! Unknown/crashed/cancelled usage keeps the reservation — never freed as if
//! it were unused. This is client accounting, not a provider invoice cap.
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetBlocked(pub String);

impl std::fmt::Display for BudgetBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BudgetBlocked {}

impl From<BudgetBlocked> for String {
    fn from(e: BudgetBlocked) -> Self {
        e.0
    }
}

/// Decimal-ceiling `value * scale` to micro units. Finite, non-negative only.
pub fn amount(value: &Value, scale: i64) -> Result<i64, BudgetBlocked> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => {
            return Err(BudgetBlocked(
                "Budget and prices must be finite nonnegative numbers".into(),
            ))
        }
    };
    if matches!(value, Value::Number(n) if n.as_f64().is_some_and(|f| !f.is_finite())) {
        return Err(BudgetBlocked(
            "Budget and prices must be finite nonnegative numbers".into(),
        ));
    }
    let number = Decimal::from_str(&text)
        .or_else(|_| Decimal::from_scientific(&text))
        .map_err(|_| {
            BudgetBlocked("Budget and prices must be finite nonnegative numbers".into())
        })?;
    if number.is_sign_negative() {
        return Err(BudgetBlocked(
            "Budget and prices must be finite nonnegative numbers".into(),
        ));
    }
    let scaled = number * Decimal::from(scale);
    let ceiled = scaled.ceil();
    ceiled
        .to_string()
        .parse::<i64>()
        .map_err(|_| BudgetBlocked("Budget and prices must be finite nonnegative numbers".into()))
}

/// Budget is opt-in accounting, not a setup requirement: a policy that is
/// present must validate against the active model; an absent or null one
/// means no ledger and no spend gate, so the daemon starts anyway.
pub fn gate(config: &Value, model: &str) -> Result<bool, BudgetBlocked> {
    match config.get("budget") {
        Some(policy) if !policy.is_null() => {
            validate(policy, model)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Validate a budget policy against the active model. Returns (daily, turn).
pub fn validate(policy: &Value, model: &str) -> Result<(i64, i64), BudgetBlocked> {
    if !policy.is_object() || policy.get("model").and_then(Value::as_str) != Some(model) {
        return Err(BudgetBlocked(
            "Configure an explicit budget and prices for the selected model".into(),
        ));
    }
    for key in ["input_per_million", "output_per_million"] {
        amount(policy.get(key).unwrap_or(&Value::Null), 1)?;
    }
    let daily = amount(policy.get("daily_usd").unwrap_or(&Value::Null), 1_000_000)?;
    let turn = amount(policy.get("turn_usd").unwrap_or(&Value::Null), 1_000_000)?;
    if daily <= 0 || turn <= 0 || turn > daily {
        return Err(BudgetBlocked(
            "Budget requires 0 < turn_usd <= daily_usd".into(),
        ));
    }
    Ok((daily, turn))
}

fn utc_day() -> String {
    chrono::Utc::now().date_naive().to_string()
}

fn connect(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|_| "cannot create budget directory".to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
    }
    let conn = Connection::open(path).map_err(|_| "cannot open budget ledger".to_string())?;
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "cannot open budget ledger".to_string())?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS reservations (id TEXT PRIMARY KEY, day TEXT, amount INTEGER, settled INTEGER DEFAULT 0)")
        .map_err(|_| "cannot open budget ledger".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(conn)
}

/// Persistent per-run reservations in a SQLite ledger.
pub struct Budget {
    path: std::path::PathBuf,
}

impl Budget {
    pub fn new(path: &Path) -> Result<Self, String> {
        connect(path)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn reserve(&self, id: &str, policy: &Value, model: &str) -> Result<i64, BudgetBlocked> {
        let (daily, turn) = validate(policy, model)?;
        let day = utc_day();
        let conn = connect(&self.path).map_err(BudgetBlocked)?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| BudgetBlocked(e.to_string()))?;
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM reservations WHERE id=?1",
                params![id],
                |_| Ok(true),
            )
            .optional()
            .map_err(|e| BudgetBlocked(e.to_string()))?
            .unwrap_or(false);
        if exists {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(BudgetBlocked(
                "This run already has a budget reservation; do not replay it".into(),
            ));
        }
        let used: i64 = conn
            .query_row(
                "SELECT coalesce(sum(amount),0) FROM reservations WHERE day=?1 OR settled=0",
                params![day],
                |r| r.get(0),
            )
            .map_err(|e| BudgetBlocked(e.to_string()))?;
        if used + turn > daily {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(BudgetBlocked(
                "Daily budget exhausted or reserved by unfinished work".into(),
            ));
        }
        conn.execute(
            "INSERT INTO reservations(id,day,amount) VALUES(?1,?2,?3)",
            params![id, day, turn],
        )
        .map_err(|e| BudgetBlocked(e.to_string()))?;
        conn.execute_batch("COMMIT")
            .map_err(|e| BudgetBlocked(e.to_string()))?;
        Ok(turn)
    }

    /// Settle a reservation to its real cost. Missing/incomplete/negative
    /// usage keeps the full reservation.
    pub fn settle(&self, id: &str, accounting: &Value) {
        let complete = accounting
            .get("usage_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cost = accounting.get("cost_micros").and_then(Value::as_i64);
        let (true, Some(cost)) = (complete, cost) else {
            return;
        };
        if cost < 0 {
            return;
        }
        let Ok(conn) = connect(&self.path) else {
            return;
        };
        let _ = conn.execute(
            "UPDATE reservations SET amount=?1,settled=1 WHERE id=?2 AND settled=0",
            params![cost, id],
        );
    }

    pub fn total(&self) -> i64 {
        connect(&self.path)
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT coalesce(sum(amount),0) FROM reservations",
                    [],
                    |r| r.get(0),
                )
                .ok()
            })
            .unwrap_or(0)
    }
}

trait Optional<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}
impl<T> Optional<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
