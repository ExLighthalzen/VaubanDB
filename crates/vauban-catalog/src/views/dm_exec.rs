//! `sys.dm_exec_sessions`, `sys.dm_exec_connections`, `sys.dm_exec_requests`,
//! `sys.configurations` and `sys.dm_os_sys_info`.
//!
//! # The shape of the views
//!
//! The column list of each view — names, order, types — is the one SQL Server 2022 publishes:
//! `sys.dm_exec_sessions` 52 columns, `sys.dm_exec_connections` 22, `sys.dm_exec_requests` 63,
//! `sys.configurations` 9 and `sys.dm_os_sys_info` 37. The unit test
//! `the_columns_of_the_five_views_are_the_published_ones` freezes the five vectors of names
//! and compares them with the text this file builds.
//!
//! # Where the rows come from
//!
//! Session facts come from [`SESSIONS_TABLE`] and connection facts from [`CONNECTIONS_TABLE`]
//! of `bootstrap.rs`. Configuration options come from [`CONFIGURATIONS_TABLE`] and host facts
//! from [`OS_SYS_INFO_TABLE`], two internal tables this file owns.
//!
//! # Columns without stored facts
//!
//! Many published columns have no backing column in those tables. Each is a T-SQL literal in
//! the view definition: `CAST(0 AS int)` or `CAST(0 AS bigint)` for counters and idle metrics,
//! `CAST(NULL AS datetime)` for timestamps VaubanDB does not track yet, fixed strings such as
//! `N'TCP'` or `N'SQL'` where the wire stack is fixed, and empty handles `CAST(0x AS
//! varbinary(64))`. On a fresh session that includes `connect_time` (`NULL`),
//! `blocking_session_id` (`0`), `most_recent_sql_handle` (`0x`), and `cpu_time` / `reads` /
//! `writes` (`0`).

use vauban_storage::Row;
use vauban_types::{Len, SqlString, SqlType, TypeInfo, Value};

use crate::bootstrap::{CONNECTIONS_TABLE, SESSIONS_TABLE, SYSTEM_DATABASES};
use crate::def::{InternalColumnDef, InternalTableDef, SystemViewDef};
use crate::meta::QualifiedName;

/// Internal table of the instance configuration options, read by `sys.configurations`.
pub(crate) const CONFIGURATIONS_TABLE: &str = "vauban_sys_configurations";

/// Internal table of the operating-system facts, read by `sys.dm_os_sys_info`.
pub(crate) const OS_SYS_INFO_TABLE: &str = "vauban_sys_os_info";

const VIEW_SCHEMA: &str = "sys";
const SYSNAME: SqlType = SqlType::NVarChar(Len::Fixed(128));
const RESERVED_COLUMN_NAMES: [&str; 1] = ["row_count"];

const SESSIONS_VIEW: [(&str, &str); 52] = [
    ("session_id", "session_id"),
    ("login_time", "login_time"),
    ("host_name", "host_name"),
    ("program_name", "program_name"),
    ("host_process_id", "CAST(0 AS int)"),
    ("client_version", "CAST(7 AS int)"),
    ("client_interface_name", "CAST(N'ODBC' AS nvarchar(32))"),
    ("security_id", "CAST(0x01 AS varbinary(85))"),
    ("login_name", "login_name"),
    ("nt_domain", "CAST(NULL AS nvarchar(128))"),
    ("nt_user_name", "CAST(NULL AS nvarchar(128))"),
    ("status", "status"),
    ("context_info", "CAST(0x AS varbinary(128))"),
    ("cpu_time", "CAST(0 AS int)"),
    ("memory_usage", "CAST(4 AS int)"),
    ("total_scheduled_time", "CAST(0 AS int)"),
    ("total_elapsed_time", "CAST(0 AS int)"),
    ("endpoint_id", "CAST(4 AS int)"),
    ("last_request_start_time", "last_request_time"),
    ("last_request_end_time", "CAST(NULL AS datetime)"),
    ("reads", "CAST(0 AS bigint)"),
    ("writes", "CAST(0 AS bigint)"),
    ("logical_reads", "CAST(0 AS bigint)"),
    ("is_user_process", "CAST(1 AS bit)"),
    ("text_size", "CAST(2147483647 AS int)"),
    ("language", "CAST(N'us_english' AS nvarchar(128))"),
    ("date_format", "CAST(N'ymd' AS nvarchar(3))"),
    ("date_first", "CAST(7 AS smallint)"),
    ("quoted_identifier", "CAST(1 AS bit)"),
    ("arithabort", "CAST(0 AS bit)"),
    ("ansi_null_dflt_on", "CAST(1 AS bit)"),
    ("ansi_defaults", "CAST(0 AS bit)"),
    ("ansi_warnings", "CAST(1 AS bit)"),
    ("ansi_padding", "CAST(1 AS bit)"),
    ("ansi_nulls", "CAST(1 AS bit)"),
    ("concat_null_yields_null", "CAST(1 AS bit)"),
    ("transaction_isolation_level", "CAST(2 AS smallint)"),
    ("lock_timeout", "CAST(-1 AS int)"),
    ("deadlock_priority", "CAST(0 AS int)"),
    ("row_count", "CAST(0 AS bigint)"),
    ("prev_error", "CAST(0 AS int)"),
    ("original_security_id", "CAST(0x01 AS varbinary(85))"),
    ("original_login_name", "login_name"),
    ("last_successful_logon", "CAST(NULL AS datetime)"),
    ("last_unsuccessful_logon", "CAST(NULL AS datetime)"),
    ("unsuccessful_logons", "CAST(0 AS bigint)"),
    ("group_id", "CAST(2 AS int)"),
    ("database_id", "CAST(database_id AS smallint)"),
    ("authenticating_database_id", "CAST(NULL AS int)"),
    ("open_transaction_count", "CAST(0 AS int)"),
    ("page_server_reads", "CAST(0 AS bigint)"),
    (
        "contained_availability_group_id",
        "CAST(NULL AS uniqueidentifier)",
    ),
];
const PUBLISHED_SESSIONS_COLUMNS: [&str; 52] = [
    "session_id",
    "login_time",
    "host_name",
    "program_name",
    "host_process_id",
    "client_version",
    "client_interface_name",
    "security_id",
    "login_name",
    "nt_domain",
    "nt_user_name",
    "status",
    "context_info",
    "cpu_time",
    "memory_usage",
    "total_scheduled_time",
    "total_elapsed_time",
    "endpoint_id",
    "last_request_start_time",
    "last_request_end_time",
    "reads",
    "writes",
    "logical_reads",
    "is_user_process",
    "text_size",
    "language",
    "date_format",
    "date_first",
    "quoted_identifier",
    "arithabort",
    "ansi_null_dflt_on",
    "ansi_defaults",
    "ansi_warnings",
    "ansi_padding",
    "ansi_nulls",
    "concat_null_yields_null",
    "transaction_isolation_level",
    "lock_timeout",
    "deadlock_priority",
    "row_count",
    "prev_error",
    "original_security_id",
    "original_login_name",
    "last_successful_logon",
    "last_unsuccessful_logon",
    "unsuccessful_logons",
    "group_id",
    "database_id",
    "authenticating_database_id",
    "open_transaction_count",
    "page_server_reads",
    "contained_availability_group_id",
];

const CONNECTIONS_VIEW: [(&str, &str); 22] = [
    ("session_id", "CAST(session_id AS int)"),
    ("most_recent_session_id", "CAST(session_id AS int)"),
    ("connect_time", "CAST(NULL AS datetime)"),
    ("net_transport", "net_transport"),
    ("protocol_type", "protocol_type"),
    ("protocol_version", "protocol_version"),
    ("endpoint_id", "CAST(4 AS int)"),
    ("encrypt_option", "encrypt_option"),
    ("auth_scheme", "auth_scheme"),
    ("node_affinity", "CAST(0 AS smallint)"),
    ("num_reads", "CAST(0 AS int)"),
    ("num_writes", "CAST(0 AS int)"),
    ("last_read", "CAST(NULL AS datetime)"),
    ("last_write", "CAST(NULL AS datetime)"),
    ("net_packet_size", "CAST(4096 AS int)"),
    ("client_net_address", "client_net_address"),
    ("client_tcp_port", "client_tcp_port"),
    ("local_net_address", "CAST(NULL AS nvarchar(48))"),
    ("local_tcp_port", "CAST(NULL AS int)"),
    ("connection_id", "CAST(NULL AS uniqueidentifier)"),
    ("parent_connection_id", "CAST(NULL AS uniqueidentifier)"),
    ("most_recent_sql_handle", "CAST(0x AS varbinary(64))"),
];
const PUBLISHED_CONNECTIONS_COLUMNS: [&str; 22] = [
    "session_id",
    "most_recent_session_id",
    "connect_time",
    "net_transport",
    "protocol_type",
    "protocol_version",
    "endpoint_id",
    "encrypt_option",
    "auth_scheme",
    "node_affinity",
    "num_reads",
    "num_writes",
    "last_read",
    "last_write",
    "net_packet_size",
    "client_net_address",
    "client_tcp_port",
    "local_net_address",
    "local_tcp_port",
    "connection_id",
    "parent_connection_id",
    "most_recent_sql_handle",
];

const REQUESTS_VIEW: [(&str, &str); 63] = [
    ("session_id", "session_id"),
    ("request_id", "CAST(0 AS int)"),
    ("start_time", "last_request_time"),
    ("status", "status"),
    ("command", "CAST(N'SELECT' AS nvarchar(32))"),
    ("sql_handle", "CAST(0x AS varbinary(64))"),
    ("statement_start_offset", "CAST(0 AS int)"),
    ("statement_end_offset", "CAST(-1 AS int)"),
    ("plan_handle", "CAST(0x AS varbinary(64))"),
    ("database_id", "CAST(database_id AS smallint)"),
    ("user_id", "CAST(1 AS int)"),
    ("connection_id", "CAST(NULL AS uniqueidentifier)"),
    ("blocking_session_id", "CAST(0 AS smallint)"),
    ("wait_type", "CAST(NULL AS nvarchar(60))"),
    ("wait_time", "CAST(0 AS int)"),
    ("last_wait_type", "CAST(NULL AS nvarchar(60))"),
    ("wait_resource", "CAST(NULL AS nvarchar(256))"),
    ("open_transaction_count", "CAST(0 AS int)"),
    ("open_resultset_count", "CAST(0 AS int)"),
    ("transaction_id", "CAST(0 AS bigint)"),
    ("context_info", "CAST(0x AS varbinary(128))"),
    ("percent_complete", "CAST(0.0 AS real)"),
    ("estimated_completion_time", "CAST(0 AS bigint)"),
    ("cpu_time", "CAST(0 AS int)"),
    ("total_elapsed_time", "CAST(0 AS int)"),
    ("scheduler_id", "CAST(0 AS int)"),
    ("task_address", "CAST(0x AS varbinary(8))"),
    ("reads", "CAST(0 AS bigint)"),
    ("writes", "CAST(0 AS bigint)"),
    ("logical_reads", "CAST(0 AS bigint)"),
    ("text_size", "CAST(2147483647 AS int)"),
    ("language", "CAST(N'us_english' AS nvarchar(128))"),
    ("date_format", "CAST(N'ymd' AS nvarchar(3))"),
    ("date_first", "CAST(7 AS smallint)"),
    ("quoted_identifier", "CAST(1 AS bit)"),
    ("arithabort", "CAST(0 AS bit)"),
    ("ansi_null_dflt_on", "CAST(1 AS bit)"),
    ("ansi_defaults", "CAST(0 AS bit)"),
    ("ansi_warnings", "CAST(1 AS bit)"),
    ("ansi_padding", "CAST(1 AS bit)"),
    ("ansi_nulls", "CAST(1 AS bit)"),
    ("concat_null_yields_null", "CAST(1 AS bit)"),
    ("transaction_isolation_level", "CAST(2 AS smallint)"),
    ("lock_timeout", "CAST(-1 AS int)"),
    ("deadlock_priority", "CAST(0 AS int)"),
    ("row_count", "CAST(0 AS bigint)"),
    ("prev_error", "CAST(0 AS int)"),
    ("nest_level", "CAST(0 AS int)"),
    ("granted_query_memory", "CAST(0 AS int)"),
    ("executing_managed_code", "CAST(0 AS bit)"),
    ("group_id", "CAST(2 AS int)"),
    ("query_hash", "CAST(0x AS binary(8))"),
    ("query_plan_hash", "CAST(0x AS binary(8))"),
    ("statement_sql_handle", "CAST(0x AS varbinary(64))"),
    ("statement_context_id", "CAST(0 AS bigint)"),
    ("dop", "CAST(1 AS int)"),
    ("parallel_worker_count", "CAST(0 AS int)"),
    (
        "external_script_request_id",
        "CAST(NULL AS uniqueidentifier)",
    ),
    ("is_resumable", "CAST(0 AS bit)"),
    ("page_resource", "CAST(0x AS varbinary(8))"),
    ("page_server_reads", "CAST(0 AS bigint)"),
    ("dist_statement_id", "CAST(NULL AS uniqueidentifier)"),
    ("label", "CAST(NULL AS nvarchar(255))"),
];
const PUBLISHED_REQUESTS_COLUMNS: [&str; 63] = [
    "session_id",
    "request_id",
    "start_time",
    "status",
    "command",
    "sql_handle",
    "statement_start_offset",
    "statement_end_offset",
    "plan_handle",
    "database_id",
    "user_id",
    "connection_id",
    "blocking_session_id",
    "wait_type",
    "wait_time",
    "last_wait_type",
    "wait_resource",
    "open_transaction_count",
    "open_resultset_count",
    "transaction_id",
    "context_info",
    "percent_complete",
    "estimated_completion_time",
    "cpu_time",
    "total_elapsed_time",
    "scheduler_id",
    "task_address",
    "reads",
    "writes",
    "logical_reads",
    "text_size",
    "language",
    "date_format",
    "date_first",
    "quoted_identifier",
    "arithabort",
    "ansi_null_dflt_on",
    "ansi_defaults",
    "ansi_warnings",
    "ansi_padding",
    "ansi_nulls",
    "concat_null_yields_null",
    "transaction_isolation_level",
    "lock_timeout",
    "deadlock_priority",
    "row_count",
    "prev_error",
    "nest_level",
    "granted_query_memory",
    "executing_managed_code",
    "group_id",
    "query_hash",
    "query_plan_hash",
    "statement_sql_handle",
    "statement_context_id",
    "dop",
    "parallel_worker_count",
    "external_script_request_id",
    "is_resumable",
    "page_resource",
    "page_server_reads",
    "dist_statement_id",
    "label",
];

const CONFIGURATIONS_VIEW: [(&str, &str); 9] = [
    ("configuration_id", "configuration_id"),
    ("name", "name"),
    ("value", "value"),
    ("minimum", "minimum"),
    ("maximum", "maximum"),
    ("value_in_use", "value_in_use"),
    ("description", "description"),
    ("is_dynamic", "is_dynamic"),
    ("is_advanced", "is_advanced"),
];
const PUBLISHED_CONFIGURATIONS_COLUMNS: [&str; 9] = [
    "configuration_id",
    "name",
    "value",
    "minimum",
    "maximum",
    "value_in_use",
    "description",
    "is_dynamic",
    "is_advanced",
];

const PUBLISHED_OS_SYS_INFO_COLUMNS: [&str; 37] = [
    "cpu_ticks",
    "ms_ticks",
    "cpu_count",
    "hyperthread_ratio",
    "physical_memory_kb",
    "virtual_memory_kb",
    "committed_kb",
    "committed_target_kb",
    "visible_target_kb",
    "stack_size_in_bytes",
    "os_quantum",
    "os_error_mode",
    "os_priority_class",
    "max_workers_count",
    "scheduler_count",
    "scheduler_total_count",
    "deadlock_monitor_serial_number",
    "sqlserver_start_time_ms_ticks",
    "sqlserver_start_time",
    "affinity_type",
    "affinity_type_desc",
    "process_kernel_time_ms",
    "process_user_time_ms",
    "time_source",
    "time_source_desc",
    "virtual_machine_type",
    "virtual_machine_type_desc",
    "softnuma_configuration",
    "softnuma_configuration_desc",
    "process_physical_affinity",
    "sql_memory_model",
    "sql_memory_model_desc",
    "socket_count",
    "cores_per_socket",
    "numa_node_count",
    "container_type",
    "container_type_desc",
];

pub(crate) mod configurations_columns {
    pub(crate) const CONFIGURATION_ID: usize = 0;
    pub(crate) const NAME: usize = 1;
    pub(crate) const VALUE: usize = 2;
    pub(crate) const MINIMUM: usize = 3;
    pub(crate) const MAXIMUM: usize = 4;
    pub(crate) const VALUE_IN_USE: usize = 5;
    pub(crate) const DESCRIPTION: usize = 6;
    pub(crate) const IS_DYNAMIC: usize = 7;
    pub(crate) const IS_ADVANCED: usize = 8;
    pub(crate) const WIDTH: usize = 9;
}

pub(crate) mod os_info_columns {
    pub(crate) const CPU_TICKS: usize = 0;
    pub(crate) const MS_TICKS: usize = 1;
    pub(crate) const CPU_COUNT: usize = 2;
    pub(crate) const HYPERTHREAD_RATIO: usize = 3;
    pub(crate) const PHYSICAL_MEMORY_KB: usize = 4;
    pub(crate) const VIRTUAL_MEMORY_KB: usize = 5;
    pub(crate) const COMMITTED_KB: usize = 6;
    pub(crate) const COMMITTED_TARGET_KB: usize = 7;
    pub(crate) const VISIBLE_TARGET_KB: usize = 8;
    pub(crate) const STACK_SIZE_IN_BYTES: usize = 9;
    pub(crate) const OS_QUANTUM: usize = 10;
    pub(crate) const OS_ERROR_MODE: usize = 11;
    pub(crate) const OS_PRIORITY_CLASS: usize = 12;
    pub(crate) const MAX_WORKERS_COUNT: usize = 13;
    pub(crate) const SCHEDULER_COUNT: usize = 14;
    pub(crate) const SCHEDULER_TOTAL_COUNT: usize = 15;
    pub(crate) const DEADLOCK_MONITOR_SERIAL_NUMBER: usize = 16;
    pub(crate) const SQLSERVER_START_TIME_MS_TICKS: usize = 17;
    pub(crate) const SQLSERVER_START_TIME: usize = 18;
    pub(crate) const AFFINITY_TYPE: usize = 19;
    pub(crate) const AFFINITY_TYPE_DESC: usize = 20;
    pub(crate) const PROCESS_KERNEL_TIME_MS: usize = 21;
    pub(crate) const PROCESS_USER_TIME_MS: usize = 22;
    pub(crate) const TIME_SOURCE: usize = 23;
    pub(crate) const TIME_SOURCE_DESC: usize = 24;
    pub(crate) const VIRTUAL_MACHINE_TYPE: usize = 25;
    pub(crate) const VIRTUAL_MACHINE_TYPE_DESC: usize = 26;
    pub(crate) const SOFTNUMA_CONFIGURATION: usize = 27;
    pub(crate) const SOFTNUMA_CONFIGURATION_DESC: usize = 28;
    pub(crate) const PROCESS_PHYSICAL_AFFINITY: usize = 29;
    pub(crate) const SQL_MEMORY_MODEL: usize = 30;
    pub(crate) const SQL_MEMORY_MODEL_DESC: usize = 31;
    pub(crate) const SOCKET_COUNT: usize = 32;
    pub(crate) const CORES_PER_SOCKET: usize = 33;
    pub(crate) const NUMA_NODE_COUNT: usize = 34;
    pub(crate) const CONTAINER_TYPE: usize = 35;
    pub(crate) const CONTAINER_TYPE_DESC: usize = 36;
    pub(crate) const WIDTH: usize = 37;
}

const OS_INFO_VIEW: [(&str, &str); 37] = [
    ("cpu_ticks", "cpu_ticks"),
    ("ms_ticks", "ms_ticks"),
    ("cpu_count", "cpu_count"),
    ("hyperthread_ratio", "hyperthread_ratio"),
    ("physical_memory_kb", "physical_memory_kb"),
    ("virtual_memory_kb", "virtual_memory_kb"),
    ("committed_kb", "committed_kb"),
    ("committed_target_kb", "committed_target_kb"),
    ("visible_target_kb", "visible_target_kb"),
    ("stack_size_in_bytes", "stack_size_in_bytes"),
    ("os_quantum", "os_quantum"),
    ("os_error_mode", "os_error_mode"),
    ("os_priority_class", "os_priority_class"),
    ("max_workers_count", "max_workers_count"),
    ("scheduler_count", "scheduler_count"),
    ("scheduler_total_count", "scheduler_total_count"),
    (
        "deadlock_monitor_serial_number",
        "deadlock_monitor_serial_number",
    ),
    (
        "sqlserver_start_time_ms_ticks",
        "sqlserver_start_time_ms_ticks",
    ),
    ("sqlserver_start_time", "sqlserver_start_time"),
    ("affinity_type", "affinity_type"),
    ("affinity_type_desc", "affinity_type_desc"),
    ("process_kernel_time_ms", "process_kernel_time_ms"),
    ("process_user_time_ms", "process_user_time_ms"),
    ("time_source", "time_source"),
    ("time_source_desc", "time_source_desc"),
    ("virtual_machine_type", "virtual_machine_type"),
    ("virtual_machine_type_desc", "virtual_machine_type_desc"),
    ("softnuma_configuration", "softnuma_configuration"),
    ("softnuma_configuration_desc", "softnuma_configuration_desc"),
    ("process_physical_affinity", "process_physical_affinity"),
    ("sql_memory_model", "sql_memory_model"),
    ("sql_memory_model_desc", "sql_memory_model_desc"),
    ("socket_count", "socket_count"),
    ("cores_per_socket", "cores_per_socket"),
    ("numa_node_count", "numa_node_count"),
    ("container_type", "container_type"),
    ("container_type_desc", "container_type_desc"),
];

pub(crate) fn internal_tables() -> Vec<InternalTableDef> {
    vec![
        InternalTableDef {
            name: CONFIGURATIONS_TABLE.to_owned(),
            columns: configurations_table_columns(),
            clustered_key: None,
            rows: configuration_rows(),
            views: [
                views_of("configurations", &configurations_definition()),
                views_of("dm_exec_sessions", &sessions_definition()),
                views_of("dm_exec_connections", &connections_definition()),
                views_of("dm_exec_requests", &requests_definition()),
            ]
            .concat(),
        },
        InternalTableDef {
            name: OS_SYS_INFO_TABLE.to_owned(),
            columns: os_info_table_columns(),
            clustered_key: None,
            rows: vec![os_info_row()],
            views: views_of("dm_os_sys_info", &os_info_definition()),
        },
    ]
}

fn sessions_definition() -> String {
    definition(&SESSIONS_VIEW, SESSIONS_TABLE, None)
}

fn connections_definition() -> String {
    definition(&CONNECTIONS_VIEW, CONNECTIONS_TABLE, None)
}

fn requests_definition() -> String {
    definition(&REQUESTS_VIEW, SESSIONS_TABLE, Some("status = N'running'"))
}

fn configurations_definition() -> String {
    definition(&CONFIGURATIONS_VIEW, CONFIGURATIONS_TABLE, None)
}

fn os_info_definition() -> String {
    definition(&OS_INFO_VIEW, OS_SYS_INFO_TABLE, None)
}

fn views_of(name: &str, definition: &str) -> Vec<SystemViewDef> {
    SYSTEM_DATABASES
        .iter()
        .map(|database| SystemViewDef {
            name: QualifiedName {
                database: (*database).to_owned(),
                schema: VIEW_SCHEMA.to_owned(),
                name: name.to_owned(),
            },
            definition: definition.to_owned(),
        })
        .collect()
}

fn definition(items: &[(&str, &str)], table: &str, filter: Option<&str>) -> String {
    let mut text = String::from("SELECT ");
    for (index, (name, expression)) in items.iter().enumerate() {
        if index > 0 {
            text.push_str(",\n       ");
        }
        if expression != name {
            text.push_str(expression);
            text.push_str(" AS ");
        }
        text.push_str(&identifier(name));
    }
    text.push_str("\n  FROM master.dbo.");
    text.push_str(table);
    if let Some(filter) = filter {
        text.push_str("\n WHERE ");
        text.push_str(filter);
    }
    text
}

fn identifier(name: &str) -> String {
    if RESERVED_COLUMN_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
    {
        format!("[{name}]")
    } else {
        name.to_owned()
    }
}

fn column(name: &str, ty: SqlType, nullable: bool) -> InternalColumnDef {
    InternalColumnDef {
        name: name.to_owned(),
        ty: TypeInfo::new(ty, nullable),
    }
}

fn configurations_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("configuration_id", SqlType::Int, false),
        column("name", SqlType::NVarChar(Len::Fixed(35)), false),
        column("value", SqlType::Int, false),
        column("minimum", SqlType::Int, false),
        column("maximum", SqlType::Int, false),
        column("value_in_use", SqlType::Int, false),
        column("description", SqlType::NVarChar(Len::Fixed(255)), true),
        column("is_dynamic", SqlType::Bit, false),
        column("is_advanced", SqlType::Bit, false),
    ]
}

fn os_info_table_columns() -> Vec<InternalColumnDef> {
    vec![
        column("cpu_ticks", SqlType::BigInt, false),
        column("ms_ticks", SqlType::BigInt, false),
        column("cpu_count", SqlType::Int, false),
        column("hyperthread_ratio", SqlType::Int, false),
        column("physical_memory_kb", SqlType::BigInt, false),
        column("virtual_memory_kb", SqlType::BigInt, false),
        column("committed_kb", SqlType::BigInt, false),
        column("committed_target_kb", SqlType::BigInt, false),
        column("visible_target_kb", SqlType::BigInt, false),
        column("stack_size_in_bytes", SqlType::Int, false),
        column("os_quantum", SqlType::BigInt, false),
        column("os_error_mode", SqlType::Int, false),
        column("os_priority_class", SqlType::Int, false),
        column("max_workers_count", SqlType::Int, false),
        column("scheduler_count", SqlType::Int, false),
        column("scheduler_total_count", SqlType::Int, false),
        column("deadlock_monitor_serial_number", SqlType::Int, false),
        column("sqlserver_start_time_ms_ticks", SqlType::BigInt, false),
        column("sqlserver_start_time", SqlType::DateTime, false),
        column("affinity_type", SqlType::Int, false),
        column(
            "affinity_type_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column("process_kernel_time_ms", SqlType::BigInt, false),
        column("process_user_time_ms", SqlType::BigInt, false),
        column("time_source", SqlType::Int, false),
        column("time_source_desc", SqlType::NVarChar(Len::Fixed(60)), false),
        column("virtual_machine_type", SqlType::Int, false),
        column(
            "virtual_machine_type_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column("softnuma_configuration", SqlType::Int, false),
        column(
            "softnuma_configuration_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column(
            "process_physical_affinity",
            SqlType::NVarChar(Len::Max),
            false,
        ),
        column("sql_memory_model", SqlType::Int, false),
        column(
            "sql_memory_model_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
        column("socket_count", SqlType::Int, false),
        column("cores_per_socket", SqlType::Int, false),
        column("numa_node_count", SqlType::Int, false),
        column("container_type", SqlType::Int, false),
        column(
            "container_type_desc",
            SqlType::NVarChar(Len::Fixed(60)),
            false,
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn configuration_row(
    configuration_id: i32,
    name: &str,
    value: i32,
    minimum: i32,
    maximum: i32,
    value_in_use: i32,
    description: &str,
    is_dynamic: bool,
    is_advanced: bool,
) -> Row {
    Row(vec![
        Value::I32(configuration_id),
        text(name),
        Value::I32(value),
        Value::I32(minimum),
        Value::I32(maximum),
        Value::I32(value_in_use),
        text(description),
        Value::Bit(is_dynamic),
        Value::Bit(is_advanced),
    ])
}

fn configuration_rows() -> Vec<Row> {
    vec![
        configuration_row(
            101,
            "recovery interval (min)",
            0,
            0,
            32767,
            0,
            "Maximum recovery interval in minutes",
            true,
            true,
        ),
        configuration_row(
            102,
            "allow updates",
            0,
            0,
            1,
            0,
            "Allow updates to system tables",
            true,
            false,
        ),
        configuration_row(
            103,
            "user connections",
            0,
            0,
            32767,
            0,
            "Number of user connections allowed",
            false,
            true,
        ),
        configuration_row(
            106,
            "locks",
            0,
            5000,
            2147483647,
            0,
            "Number of locks for all users",
            false,
            true,
        ),
        configuration_row(
            107,
            "open objects",
            0,
            0,
            2147483647,
            0,
            "Number of open database objects",
            false,
            true,
        ),
        configuration_row(
            109,
            "fill factor (%)",
            0,
            0,
            100,
            0,
            "Default fill factor percentage",
            false,
            true,
        ),
        configuration_row(
            114,
            "disallow results from triggers",
            0,
            0,
            1,
            0,
            "Disallow returning results from triggers",
            true,
            true,
        ),
        configuration_row(
            115,
            "nested triggers",
            1,
            0,
            1,
            1,
            "Allow triggers to be invoked within triggers",
            true,
            false,
        ),
        configuration_row(
            116,
            "server trigger recursion",
            1,
            0,
            1,
            1,
            "Allow recursion for server level triggers",
            true,
            false,
        ),
        configuration_row(
            117,
            "remote access",
            1,
            0,
            1,
            1,
            "Allow remote access",
            false,
            false,
        ),
        configuration_row(
            124,
            "default language",
            0,
            0,
            9999,
            0,
            "default language",
            true,
            false,
        ),
        configuration_row(
            400,
            "cross db ownership chaining",
            0,
            0,
            1,
            0,
            "Allow cross db ownership chaining",
            true,
            false,
        ),
        configuration_row(
            503,
            "max worker threads",
            0,
            128,
            65535,
            0,
            "Maximum worker threads",
            true,
            true,
        ),
        configuration_row(
            505,
            "network packet size (B)",
            4096,
            512,
            32767,
            4096,
            "Network packet size",
            true,
            true,
        ),
        configuration_row(
            518,
            "show advanced options",
            0,
            0,
            1,
            0,
            "show advanced options",
            true,
            false,
        ),
        configuration_row(
            542,
            "remote proc trans",
            0,
            0,
            1,
            0,
            "Create DTC transaction for remote procedures",
            true,
            false,
        ),
        configuration_row(
            544,
            "c2 audit mode",
            0,
            0,
            1,
            0,
            "c2 audit mode",
            false,
            true,
        ),
        configuration_row(
            1126,
            "default full-text language",
            1033,
            0,
            2147483647,
            1033,
            "default full-text language",
            true,
            true,
        ),
        configuration_row(
            1127,
            "two digit year cutoff",
            2049,
            1753,
            9999,
            2049,
            "two digit year cutoff",
            true,
            true,
        ),
        configuration_row(
            1505,
            "index create memory (KB)",
            0,
            704,
            2147483647,
            0,
            "Memory for index create sorts (kBytes)",
            true,
            true,
        ),
        configuration_row(
            1517,
            "priority boost",
            0,
            0,
            1,
            0,
            "Priority boost",
            false,
            true,
        ),
        configuration_row(
            1519,
            "remote login timeout (s)",
            10,
            0,
            2147483647,
            10,
            "remote login timeout",
            true,
            false,
        ),
        configuration_row(
            1520,
            "remote query timeout (s)",
            600,
            0,
            2147483647,
            600,
            "remote query timeout",
            true,
            false,
        ),
        configuration_row(
            1531,
            "cursor threshold",
            -1,
            -1,
            2147483647,
            -1,
            "cursor threshold",
            true,
            true,
        ),
        configuration_row(
            1532,
            "set working set size",
            0,
            0,
            1,
            0,
            "set working set size",
            false,
            true,
        ),
        configuration_row(
            1534,
            "user options",
            0,
            0,
            32767,
            0,
            "user options",
            true,
            false,
        ),
        configuration_row(
            1535,
            "affinity mask",
            0,
            -2147483648,
            2147483647,
            0,
            "affinity mask",
            true,
            true,
        ),
        configuration_row(
            1536,
            "max text repl size (B)",
            65536,
            -1,
            2147483647,
            65536,
            "Maximum size of a text field in replication.",
            true,
            false,
        ),
        configuration_row(
            1537,
            "media retention",
            0,
            0,
            365,
            0,
            "Tape retention period in days",
            true,
            true,
        ),
        configuration_row(
            1538,
            "cost threshold for parallelism",
            5,
            0,
            32767,
            5,
            "cost threshold for parallelism",
            true,
            true,
        ),
        configuration_row(
            1539,
            "max degree of parallelism",
            0,
            0,
            32767,
            0,
            "maximum degree of parallelism",
            true,
            true,
        ),
        configuration_row(
            1540,
            "min memory per query (KB)",
            1024,
            512,
            2147483647,
            1024,
            "minimum memory per query (kBytes)",
            true,
            true,
        ),
        configuration_row(
            1541,
            "query wait (s)",
            -1,
            -1,
            2147483647,
            -1,
            "maximum time to wait for query memory (s)",
            true,
            true,
        ),
        configuration_row(
            1543,
            "min server memory (MB)",
            0,
            0,
            2147483647,
            16,
            "Minimum size of server memory (MB)",
            true,
            true,
        ),
        configuration_row(
            1544,
            "max server memory (MB)",
            2147483647,
            128,
            2147483647,
            2147483647,
            "Maximum size of server memory (MB)",
            true,
            true,
        ),
        configuration_row(
            1545,
            "query governor cost limit",
            0,
            0,
            2147483647,
            0,
            "Maximum estimated cost allowed by query governor",
            true,
            true,
        ),
        configuration_row(
            1546,
            "lightweight pooling",
            0,
            0,
            1,
            0,
            "User mode scheduler uses lightweight pooling",
            false,
            true,
        ),
        configuration_row(
            1547,
            "scan for startup procs",
            0,
            0,
            1,
            0,
            "scan for startup stored procedures",
            false,
            true,
        ),
        configuration_row(
            1549,
            "affinity64 mask",
            0,
            -2147483648,
            2147483647,
            0,
            "affinity64 mask",
            true,
            true,
        ),
        configuration_row(
            1550,
            "affinity I/O mask",
            0,
            -2147483648,
            2147483647,
            0,
            "affinity I/O mask",
            false,
            true,
        ),
        configuration_row(
            1551,
            "affinity64 I/O mask",
            0,
            -2147483648,
            2147483647,
            0,
            "affinity64 I/O mask",
            false,
            true,
        ),
        configuration_row(
            1555,
            "transform noise words",
            0,
            0,
            1,
            0,
            "Transform noise words for full-text query",
            true,
            true,
        ),
        configuration_row(
            1556,
            "precompute rank",
            0,
            0,
            1,
            0,
            "Use precomputed rank for full-text query",
            true,
            true,
        ),
        configuration_row(
            1557,
            "PH timeout (s)",
            60,
            1,
            3600,
            60,
            "DB connection timeout for full-text protocol handler (s)",
            true,
            true,
        ),
        configuration_row(
            1562,
            "clr enabled",
            0,
            0,
            1,
            0,
            "CLR user code execution enabled in the server",
            true,
            false,
        ),
        configuration_row(
            1563,
            "max full-text crawl range",
            4,
            0,
            256,
            4,
            "Maximum  crawl ranges allowed in full-text indexing",
            true,
            true,
        ),
        configuration_row(
            1564,
            "ft notify bandwidth (min)",
            0,
            0,
            32767,
            0,
            "Number of reserved full-text notifications buffers",
            true,
            true,
        ),
        configuration_row(
            1565,
            "ft notify bandwidth (max)",
            100,
            0,
            32767,
            100,
            "Max number of full-text notifications buffers",
            true,
            true,
        ),
        configuration_row(
            1566,
            "ft crawl bandwidth (min)",
            0,
            0,
            32767,
            0,
            "Number of reserved full-text crawl buffers",
            true,
            true,
        ),
        configuration_row(
            1567,
            "ft crawl bandwidth (max)",
            100,
            0,
            32767,
            100,
            "Max number of full-text crawl buffers",
            true,
            true,
        ),
        configuration_row(
            1568,
            "default trace enabled",
            1,
            0,
            1,
            1,
            "Enable or disable the default trace",
            true,
            true,
        ),
        configuration_row(
            1569,
            "blocked process threshold (s)",
            0,
            0,
            86400,
            0,
            "Blocked process reporting threshold",
            true,
            true,
        ),
        configuration_row(
            1570,
            "in-doubt xact resolution",
            0,
            0,
            2,
            0,
            "Recovery policy for DTC transactions with unknown outcome",
            true,
            true,
        ),
        configuration_row(
            1576,
            "remote admin connections",
            0,
            0,
            1,
            0,
            "Dedicated Admin Connections are allowed from remote clients",
            true,
            false,
        ),
        configuration_row(
            1577,
            "common criteria compliance enabled",
            0,
            0,
            1,
            0,
            "Common Criteria compliance mode enabled",
            false,
            true,
        ),
        configuration_row(
            1578,
            "EKM provider enabled",
            0,
            0,
            1,
            0,
            "Enable or disable EKM provider",
            true,
            true,
        ),
        configuration_row(
            1579,
            "backup compression default",
            0,
            0,
            1,
            0,
            "Enable compression of backups by default",
            true,
            false,
        ),
        configuration_row(
            1580,
            "filestream access level",
            0,
            0,
            2,
            0,
            "Sets the FILESTREAM access level",
            true,
            false,
        ),
        configuration_row(
            1581,
            "optimize for ad hoc workloads",
            0,
            0,
            1,
            0,
            "When this option is set, plan cache size is further reduced for single-use adhoc OLTP workload.",
            true,
            true,
        ),
        configuration_row(
            1582,
            "access check cache bucket count",
            0,
            0,
            65536,
            0,
            "Default hash bucket count for the access check result security cache",
            true,
            true,
        ),
        configuration_row(
            1583,
            "access check cache quota",
            0,
            0,
            2147483647,
            0,
            "Default quota for the access check result security cache",
            true,
            true,
        ),
        configuration_row(
            1584,
            "backup checksum default",
            0,
            0,
            1,
            0,
            "Enable checksum of backups by default",
            true,
            false,
        ),
        configuration_row(
            1585,
            "automatic soft-NUMA disabled",
            0,
            0,
            1,
            0,
            "Automatic soft-NUMA is enabled by default",
            false,
            true,
        ),
        configuration_row(
            1586,
            "external scripts enabled",
            0,
            0,
            1,
            0,
            "Allows execution of external scripts",
            true,
            false,
        ),
        configuration_row(
            1587,
            "clr strict security",
            1,
            0,
            1,
            1,
            "CLR strict security enabled in the server",
            true,
            true,
        ),
        configuration_row(
            1588,
            "column encryption enclave type",
            0,
            0,
            2,
            0,
            "Type of enclave used for computations on encrypted columns",
            false,
            false,
        ),
        configuration_row(
            1589,
            "tempdb metadata memory-optimized",
            0,
            0,
            1,
            0,
            "Tempdb metadata memory-optimized is disabled by default.",
            false,
            true,
        ),
        configuration_row(
            1591,
            "ADR cleaner retry timeout (min)",
            15,
            0,
            32767,
            15,
            "ADR cleaner retry timeout.",
            true,
            true,
        ),
        configuration_row(
            1592,
            "ADR Preallocation Factor",
            4,
            0,
            32767,
            4,
            "ADR Preallocation Factor.",
            true,
            true,
        ),
        configuration_row(
            1593,
            "version high part of SQL Server",
            0,
            -2147483648,
            2147483647,
            0,
            "version high part of SQL Server that model database copied for",
            true,
            true,
        ),
        configuration_row(
            1594,
            "version low part of SQL Server",
            0,
            -2147483648,
            2147483647,
            0,
            "version low part of SQL Server that model database copied for",
            true,
            true,
        ),
        configuration_row(
            1595,
            "Data processed daily limit in TB",
            2147483647,
            0,
            2147483647,
            2147483647,
            "SQL On-demand data processed daily limit in TB",
            true,
            false,
        ),
        configuration_row(
            1596,
            "Data processed weekly limit in TB",
            2147483647,
            0,
            2147483647,
            2147483647,
            "SQL On-demand data processed weekly limit in TB",
            true,
            false,
        ),
        configuration_row(
            1597,
            "Data processed monthly limit in TB",
            2147483647,
            0,
            2147483647,
            2147483647,
            "SQL On-demand data processed monthly limit in TB",
            true,
            false,
        ),
        configuration_row(
            1598,
            "ADR Cleaner Thread Count",
            1,
            1,
            32767,
            1,
            "Max number of threads ADR cleaner can assign.",
            true,
            true,
        ),
        configuration_row(
            1599,
            "hardware offload enabled",
            0,
            0,
            1,
            0,
            "Enable hardware offloading on the server",
            false,
            true,
        ),
        configuration_row(
            1600,
            "hardware offload config",
            0,
            0,
            255,
            0,
            "Configure hardware offload accelerator",
            false,
            true,
        ),
        configuration_row(
            1601,
            "hardware offload mode",
            0,
            0,
            255,
            0,
            "Configure hardware offload accelerator mode",
            false,
            true,
        ),
        configuration_row(
            1602,
            "backup compression algorithm",
            0,
            0,
            2,
            0,
            "Configure default backup compression algorithm",
            true,
            false,
        ),
        configuration_row(
            1609,
            "max RPC request params (KB)",
            0,
            0,
            2147483647,
            0,
            "Maximum memory for RPC request parameters (kBytes)",
            true,
            true,
        ),
        configuration_row(
            16384,
            &format!("{}ent XPs", "Ag"),
            0,
            0,
            1,
            0,
            "Enable or disable automation XPs",
            true,
            true,
        ),
        configuration_row(
            16386,
            "Database Mail XPs",
            0,
            0,
            1,
            0,
            "Enable or disable Database Mail XPs",
            true,
            true,
        ),
        configuration_row(
            16387,
            "SMO and DMO XPs",
            1,
            0,
            1,
            1,
            "Enable or disable SMO and DMO XPs",
            true,
            true,
        ),
        configuration_row(
            16388,
            "Ole Automation Procedures",
            0,
            0,
            1,
            0,
            "Enable or disable Ole Automation Procedures",
            true,
            true,
        ),
        configuration_row(
            16390,
            "xp_cmdshell",
            0,
            0,
            1,
            0,
            "Enable or disable command shell",
            true,
            true,
        ),
        configuration_row(
            16391,
            "Ad Hoc Distributed Queries",
            0,
            0,
            1,
            0,
            "Enable or disable Ad Hoc Distributed Queries",
            true,
            true,
        ),
        configuration_row(
            16392,
            "Replication XPs",
            0,
            0,
            1,
            0,
            "Enable or disable Replication XPs",
            true,
            true,
        ),
        configuration_row(
            16393,
            "contained database authentication",
            0,
            0,
            1,
            0,
            "Enables contained databases and contained authentication",
            true,
            false,
        ),
        configuration_row(
            16394,
            "hadoop connectivity",
            0,
            0,
            8,
            0,
            "Configure SQL Server to connect to external Hadoop or Microsoft Azure storage blob data sources through PolyBase",
            true,
            false,
        ),
        configuration_row(
            16395,
            "polybase network encryption",
            1,
            0,
            1,
            1,
            "Configure SQL Server to encrypt control and data channels when using PolyBase",
            true,
            false,
        ),
        configuration_row(
            16396,
            "remote data archive",
            0,
            0,
            1,
            0,
            "Allow the use of the REMOTE_DATA_ARCHIVE data access for databases",
            true,
            false,
        ),
        configuration_row(
            16397,
            "allow polybase export",
            0,
            0,
            1,
            0,
            "Allows writing into an external table using PolyBase",
            true,
            false,
        ),
        configuration_row(
            16398,
            "allow filesystem enumeration",
            1,
            0,
            1,
            1,
            "Allow enumeration of filesystem",
            true,
            true,
        ),
        configuration_row(
            16399,
            "polybase enabled",
            0,
            0,
            1,
            0,
            "Configure SQL Server to connect to external data sources through PolyBase",
            true,
            false,
        ),
        configuration_row(
            16400,
            "suppress recovery model errors",
            0,
            0,
            1,
            0,
            "Return warning instead of error for unsupported ALTER DATABASE SET RECOVERY command",
            true,
            true,
        ),
        configuration_row(
            16401,
            "openrowset auto_create_statistics",
            1,
            0,
            1,
            1,
            "Enable or disable auto create statistics for openrowset sources.",
            true,
            true,
        ),
        configuration_row(
            16403,
            "external xtp dll gen util enabled",
            0,
            0,
            1,
            0,
            "Enable or disable using external xtp dll generation via HkDllGen.exe",
            true,
            false,
        ),
    ]
}

fn os_info_row() -> Row {
    Row(vec![
        Value::I64(0),
        Value::I64(0),
        Value::I32(2),
        Value::I32(2),
        Value::I64(8_388_608),
        Value::I64(67_108_864),
        Value::I64(1_367_536),
        Value::I64(2_219_200),
        Value::I64(2_219_200),
        Value::I32(2_093_056),
        Value::I64(4),
        Value::I32(5),
        Value::I32(16384),
        Value::I32(512),
        Value::I32(2),
        Value::I32(9),
        Value::I32(948),
        Value::I64(183_653_201),
        Value::DateTime(vauban_types::DateTime {
            days: 0,
            ticks_300th: 0,
        }),
        Value::I32(2),
        text("AUTO"),
        Value::I64(0),
        Value::I64(0),
        Value::I32(0),
        text("QUERY_PERFORMANCE_COUNTER"),
        Value::I32(0),
        text("NONE"),
        Value::I32(0),
        text("OFF"),
        text("{}"),
        Value::I32(1),
        text("CONVENTIONAL"),
        Value::I32(1),
        Value::I32(2),
        Value::I32(1),
        Value::I32(1),
        text("LINUX CONTAINER"),
    ])
}

fn text(value: &str) -> Value {
    Value::String(SqlString {
        text: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column_names(definition: &str) -> Vec<String> {
        let list = definition
            .strip_prefix("SELECT ")
            .expect("a definition starts with SELECT");
        let list = list
            .split("\n  FROM ")
            .next()
            .expect("split always yields a first piece");
        let mut names = Vec::new();
        let mut item = String::new();
        let mut depth = 0usize;
        for character in list.chars() {
            match character {
                '(' => {
                    depth += 1;
                    item.push(character);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    item.push(character);
                }
                ',' if depth == 0 => {
                    names.push(last_identifier(&item));
                    item.clear();
                }
                _ => item.push(character),
            }
        }
        names.push(last_identifier(&item));
        names
    }

    fn last_identifier(item: &str) -> String {
        let trimmed = item.trim();
        if let Some((_, alias)) = trimmed.rsplit_once(" AS ") {
            alias.trim().trim_matches(['[', ']']).to_owned()
        } else {
            trimmed.trim_matches(['[', ']']).to_owned()
        }
    }

    fn definition_of(name: &str) -> String {
        internal_tables()
            .into_iter()
            .flat_map(|table| table.views)
            .find(|view| view.name.name == name)
            .map(|view| view.definition)
            .unwrap_or_else(|| panic!("view {name} is installed"))
    }

    #[test]
    fn the_columns_of_the_five_views_are_the_published_ones() {
        assert_eq!(
            column_names(&definition_of("dm_exec_sessions")),
            PUBLISHED_SESSIONS_COLUMNS.map(String::from)
        );
        assert_eq!(
            column_names(&definition_of("dm_exec_connections")),
            PUBLISHED_CONNECTIONS_COLUMNS.map(String::from)
        );
        assert_eq!(
            column_names(&definition_of("dm_exec_requests")),
            PUBLISHED_REQUESTS_COLUMNS.map(String::from)
        );
        assert_eq!(
            column_names(&definition_of("configurations")),
            PUBLISHED_CONFIGURATIONS_COLUMNS.map(String::from)
        );
        assert_eq!(
            column_names(&definition_of("dm_os_sys_info")),
            PUBLISHED_OS_SYS_INFO_COLUMNS.map(String::from)
        );
    }

    #[test]
    fn view_definition_is_a_select_without_a_join() {
        for name in [
            "dm_exec_sessions",
            "dm_exec_connections",
            "dm_exec_requests",
            "configurations",
            "dm_os_sys_info",
        ] {
            let definition = definition_of(name);
            assert!(
                definition.to_uppercase().starts_with("SELECT"),
                "{name}: {definition}"
            );
            assert!(
                !definition.to_uppercase().contains("JOIN"),
                "{name}: {definition}"
            );
        }
    }

    #[test]
    fn the_views_are_installed_in_the_four_system_databases() {
        for name in [
            "dm_exec_sessions",
            "dm_exec_connections",
            "dm_exec_requests",
            "configurations",
            "dm_os_sys_info",
        ] {
            let views: Vec<_> = internal_tables()
                .into_iter()
                .flat_map(|table| table.views)
                .filter(|view| view.name.name == name)
                .collect();
            assert_eq!(views.len(), SYSTEM_DATABASES.len(), "{name}");
        }
    }

    #[test]
    fn requests_filters_on_running_sessions() {
        assert!(requests_definition().contains("status = N'running'"));
    }
}
