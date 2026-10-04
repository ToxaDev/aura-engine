//! The radio panel's station catalog: Radio Browser's stations (`rb`), what
//! is listed and what plays here (`rules`), the stations' icons (`icons`)
//! and the panel's own records (`store`).

mod icons;
mod rb;
mod rules;
mod store;

pub use rb::Query;

/// One page of a search: `{ stations, more, formatsRev }`.
#[tauri::command]
pub async fn radio_catalog_search(query: Query) -> Result<String, String> {
    rb::search(query).await.map(|v| v.to_string())
}

/// The genres, countries and languages to choose from.
#[tauri::command]
pub async fn radio_catalog_lists() -> Result<String, String> {
    rb::lists().await.map(|v| v.to_string())
}

/// The address to play a catalog station at — asked at each start of it:
/// the catalog counts its listeners so.
#[tauri::command]
pub async fn radio_station_url(uuid: String) -> Result<String, String> {
    rb::station_url(uuid).await
}

/// The key of a station's icon, kept on disk (fetched the first time it is
/// asked for); "" when there is none. The page shows it as
/// `…/radio/icon?k=<key>` of the `aura` protocol.
#[tauri::command]
pub async fn radio_icon(url: String) -> Result<String, String> {
    Ok(icons::icon(url).await)
}

/// The panel's records (null when none are kept) and the revision of the
/// formats table: a format failure remembered under an older one is tried
/// again.
#[tauri::command]
pub fn radio_store_load() -> Result<String, String> {
    let store = rb::dir().and_then(|d| store::load_from(&d));
    Ok(serde_json::json!({ "store": store, "formatsRev": rules::FORMATS_REV }).to_string())
}

/// Keep the panel's records (a JSON object).
#[tauri::command]
pub fn radio_store_save(data: String) -> Result<(), String> {
    let d = rb::dir().ok_or("no folder for the app's data")?;
    store::save_to(&d, &data)
}

/// A kept icon's bytes and content type, for the `aura` protocol.
pub fn icon_file(key: &str) -> Option<(Vec<u8>, &'static str)> {
    icons::read(key)
}
