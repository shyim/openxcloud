//! OpenXCloud desktop client: egui/eframe shell and application controllers.
//! Ports `qt/src/app/*Controller*`, `qt/src/core/Settings`, `qt/src/app/ArtworkProvider`
//! and the QML screens under `qt/qml/`. The UI reads snapshots and sends commands
//! through the contract in [`state`]; controllers run on tokio and never touch egui
//! except to request a repaint.

pub mod app;
pub mod artwork;
pub mod library;
pub mod services;
pub mod session;
pub mod settings;
pub mod state;
pub mod stream;
pub mod theme;
pub mod ui;
pub mod user_data;
