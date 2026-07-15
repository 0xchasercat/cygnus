mod client;
mod handler;
mod protocol;
mod server;
pub const DEFAULT_HOST_ADMIN_SOCKET: &str = "/var/run/cygnus/admin.sock";

pub use client::{AdminClient, invalid_request_id, request, request as request_admin};
pub use handler::StateAdminHandler;
pub use protocol::{
    ADMIN_PROTOCOL_VERSION, ActiveDeploymentView, AdminCommand, AdminData, AdminErrorCode,
    AdminFault, AdminRequest, AdminResponse, AppView, DeploymentView, LogStream,
    MAX_ADMIN_FRAME_BYTES, MAX_LOG_CHUNK_BYTES, NodeView, read_frame, valid_request_id,
    write_frame,
};
pub use server::{
    ADMIN_IO_TIMEOUT, AdminBinding, AdminHandler, AdminRole, AdminServer,
    DEFAULT_ADMIN_QUEUE_CAPACITY, DEFAULT_ADMIN_WORKERS, MAX_ADMIN_ACTOR_BYTES,
    MAX_ADMIN_APP_BYTES, MAX_ADMIN_DEPLOYMENT_BYTES, MAX_ADMIN_LIST_LIMIT, prepare_listener,
};
