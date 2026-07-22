//! Shared application state for the axum router.

use crate::automation::AutomationStore;
use crate::manager::ManagerHandle;
use crate::scenes::SceneStore;

/// Axum state: a cheaply-cloneable manager handle (a `watch::Receiver` +
/// `mpsc::Sender`), the shared automation config store (read by the engine,
/// mutated by the automation UI handlers), and the shared preset store (read
/// and mutated by the presets UI handlers).
#[derive(Clone)]
pub struct AppState {
    pub manager: ManagerHandle,
    pub automation: AutomationStore,
    pub scenes: SceneStore,
}
