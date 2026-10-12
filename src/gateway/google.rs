//! Google's services and their actions in plain words, for the owner's
//! per-service permission screens and the gateway's rules.
//!
//! Google is one connection (one sign-in with every scope), so what Iris may
//! do in each service is decided here, in the gateway: every tool of the
//! Google Workspace MCP server (`workspace-mcp@2.1.0`) belongs to a service
//! and performs one or more friendly actions ("Send email", "Create events
//! with invitations"). The owner's Allow / Ask / Off choice for an action is
//! stored in the connector's `Policy.tools` under the action's id, and each
//! service can be turned off as a whole (`Policy.off`).
//!
//! Multi-purpose tools (`manage_event` with `action: "create"`) are placed
//! by their real arguments, never by anything the agent writes about the
//! call. A tool or argument value this table does not know returns `None`
//! and falls back to the gateway's general classification (which asks for
//! anything it cannot place).

use super::policy::Category;
use serde_json::Value;

/// One action the owner can allow, ask about, or turn off.
pub struct Action {
    /// Rule key (`gmail.send`), stored in `Policy.tools`.
    pub id: &'static str,
    pub label: &'static str,
    /// The riskiest thing it does: decides its section (reads, or writes
    /// and deletes) and its default.
    pub category: Category,
}

/// One Google service, shown as its own row and permission screen.
pub struct Service {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub actions: &'static [Action],
}

const fn a(id: &'static str, label: &'static str, category: Category) -> Action {
    Action {
        id,
        label,
        category,
    }
}

use Category::{Delete, Draft, Export, Read, Send, Write};

pub const SERVICES: &[Service] = &[
    Service {
        id: "gmail",
        label: "Gmail",
        description: "Read, organize, draft and send email",
        actions: &[
            a("gmail.read", "Read email and attachments", Read),
            a("gmail.labels.read", "See labels and filters", Read),
            a("gmail.draft", "Write drafts", Draft),
            a("gmail.send", "Send, reply to and forward email", Send),
            a("gmail.organize", "Label, archive and mark email", Write),
            a("gmail.trash", "Move email to the trash", Delete),
            a("gmail.labels", "Create, edit and delete labels", Delete),
            a("gmail.filters", "Create, edit and delete filters", Delete),
            a(
                "gmail.forward_filters",
                "Filters that forward email to someone",
                Send,
            ),
        ],
    },
    Service {
        id: "calendar",
        label: "Google Calendar",
        description: "Manage calendar events and scheduling",
        actions: &[
            a("calendar.calendars.read", "See your calendars", Read),
            a("calendar.events.read", "See events", Read),
            a("calendar.freebusy", "Check availability", Read),
            a("calendar.calendars.create", "Create calendars", Write),
            a("calendar.events.create", "Create events", Write),
            a(
                "calendar.events.invite",
                "Create events with invitations",
                Send,
            ),
            a("calendar.events.update", "Update events", Write),
            a(
                "calendar.events.notify",
                "Update events and notify guests",
                Send,
            ),
            a("calendar.events.delete", "Delete events", Delete),
            a("calendar.events.rsvp", "Reply to invitations", Send),
            a(
                "calendar.status",
                "Set out of office and focus time (can decline meetings)",
                Send,
            ),
        ],
    },
    Service {
        id: "drive",
        label: "Google Drive",
        description: "Find, organize and share your files",
        actions: &[
            a("drive.read", "Find and read files", Read),
            a("drive.sharing.read", "See who has access", Read),
            a("drive.download", "Download files", Export),
            a("drive.create", "Create, upload and copy files", Write),
            a("drive.edit", "Edit, rename and move files", Write),
            a("drive.trash", "Move files to the trash", Delete),
            a("drive.share", "Share files and change who has access", Send),
        ],
    },
    Service {
        id: "docs",
        label: "Google Docs",
        description: "Read, write and edit documents",
        actions: &[
            a("docs.read", "Read documents", Read),
            a("docs.create", "Create documents and PDFs", Write),
            a("docs.edit", "Edit documents", Write),
        ],
    },
    Service {
        id: "sheets",
        label: "Google Sheets",
        description: "Read and edit spreadsheets",
        actions: &[
            a("sheets.read", "Read spreadsheets", Read),
            a("sheets.create", "Create spreadsheets", Write),
            a("sheets.edit", "Edit spreadsheets", Write),
        ],
    },
    Service {
        id: "slides",
        label: "Google Slides",
        description: "Read and edit presentations",
        actions: &[
            a("slides.read", "Read presentations", Read),
            a("slides.create", "Create presentations", Write),
            a("slides.edit", "Edit presentations", Write),
        ],
    },
    Service {
        id: "forms",
        label: "Google Forms",
        description: "Build forms and read their responses",
        actions: &[
            a("forms.read", "Read forms", Read),
            a("forms.responses", "Read responses", Read),
            a("forms.create", "Create forms", Write),
            a("forms.edit", "Edit forms", Write),
            a(
                "forms.publish",
                "Publish forms and open or close responses",
                Send,
            ),
        ],
    },
    Service {
        id: "tasks",
        label: "Google Tasks",
        description: "Keep your to-do lists",
        actions: &[
            a("tasks.read", "See tasks and lists", Read),
            a("tasks.edit", "Add, update and move tasks and lists", Write),
            a("tasks.delete", "Delete tasks and lists", Delete),
        ],
    },
    Service {
        id: "contacts",
        label: "Google Contacts",
        description: "Look up and manage your contacts",
        actions: &[
            a("contacts.read", "Look up contacts and groups", Read),
            a("contacts.edit", "Add and update contacts and groups", Write),
            a("contacts.delete", "Delete contacts and groups", Delete),
        ],
    },
    // YouTube has no tools: Iris reads subscriptions and likes for For you
    // with the same sign-in. Its switch turns that off.
    Service {
        id: "youtube",
        label: "YouTube",
        description: "Your subscriptions and likes, for For you (read only)",
        actions: &[],
    },
];

pub fn service(id: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.id == id)
}

pub fn action(id: &str) -> Option<&'static Action> {
    SERVICES.iter().flat_map(|s| s.actions).find(|a| a.id == id)
}

/// Every tool of the server's services Iris turns on: its service and
/// every action it can perform.
const TOOLS: &[(&str, &str, &[&str])] = &[
    // Gmail
    ("search_gmail_messages", "gmail", &["gmail.read"]),
    ("get_gmail_message_content", "gmail", &["gmail.read"]),
    ("get_gmail_messages_content_batch", "gmail", &["gmail.read"]),
    ("get_gmail_attachment_content", "gmail", &["gmail.read"]),
    ("get_gmail_thread_content", "gmail", &["gmail.read"]),
    ("get_gmail_threads_content_batch", "gmail", &["gmail.read"]),
    ("list_gmail_labels", "gmail", &["gmail.labels.read"]),
    ("list_gmail_filters", "gmail", &["gmail.labels.read"]),
    ("draft_gmail_message", "gmail", &["gmail.draft"]),
    ("send_gmail_message", "gmail", &["gmail.send"]),
    (
        "modify_gmail_message_labels",
        "gmail",
        &["gmail.organize", "gmail.trash"],
    ),
    (
        "batch_modify_gmail_message_labels",
        "gmail",
        &["gmail.organize", "gmail.trash"],
    ),
    ("manage_gmail_label", "gmail", &["gmail.labels"]),
    (
        "manage_gmail_filter",
        "gmail",
        &["gmail.filters", "gmail.forward_filters"],
    ),
    // Calendar
    ("list_calendars", "calendar", &["calendar.calendars.read"]),
    ("get_events", "calendar", &["calendar.events.read"]),
    ("query_freebusy", "calendar", &["calendar.freebusy"]),
    (
        "create_calendar",
        "calendar",
        &["calendar.calendars.create"],
    ),
    (
        "manage_event",
        "calendar",
        &[
            "calendar.events.create",
            "calendar.events.invite",
            "calendar.events.update",
            "calendar.events.notify",
            "calendar.events.delete",
            "calendar.events.rsvp",
        ],
    ),
    (
        "manage_out_of_office",
        "calendar",
        &["calendar.events.read", "calendar.status"],
    ),
    (
        "manage_focus_time",
        "calendar",
        &["calendar.events.read", "calendar.status"],
    ),
    // Drive
    ("search_drive_files", "drive", &["drive.read"]),
    ("get_drive_file_content", "drive", &["drive.read"]),
    ("list_drive_items", "drive", &["drive.read"]),
    (
        "get_drive_file_permissions",
        "drive",
        &["drive.sharing.read"],
    ),
    (
        "check_drive_file_public_access",
        "drive",
        &["drive.sharing.read"],
    ),
    ("get_drive_shareable_link", "drive", &["drive.sharing.read"]),
    ("get_drive_file_download_url", "drive", &["drive.download"]),
    ("create_drive_folder", "drive", &["drive.create"]),
    ("create_drive_file", "drive", &["drive.create"]),
    ("import_to_google_doc", "drive", &["drive.create"]),
    ("import_to_google_slides", "drive", &["drive.create"]),
    ("import_to_google_sheets", "drive", &["drive.create"]),
    ("copy_drive_file", "drive", &["drive.create"]),
    (
        "update_drive_file",
        "drive",
        &["drive.edit", "drive.trash", "drive.share"],
    ),
    ("manage_drive_access", "drive", &["drive.share"]),
    ("set_drive_file_permissions", "drive", &["drive.share"]),
    // Docs
    ("search_docs", "docs", &["docs.read"]),
    ("get_doc_content", "docs", &["docs.read"]),
    ("list_docs_in_folder", "docs", &["docs.read"]),
    ("inspect_doc_structure", "docs", &["docs.read"]),
    ("debug_docs_runtime_info", "docs", &["docs.read"]),
    ("debug_table_structure", "docs", &["docs.read"]),
    ("get_doc_as_markdown", "docs", &["docs.read"]),
    ("create_doc", "docs", &["docs.create"]),
    ("export_doc_to_pdf", "docs", &["docs.create"]),
    ("modify_doc_text", "docs", &["docs.edit"]),
    ("find_and_replace_doc", "docs", &["docs.edit"]),
    ("insert_doc_elements", "docs", &["docs.edit"]),
    ("insert_doc_image", "docs", &["docs.edit"]),
    ("update_doc_headers_footers", "docs", &["docs.edit"]),
    ("batch_update_doc", "docs", &["docs.edit"]),
    ("create_table_with_data", "docs", &["docs.edit"]),
    ("update_paragraph_style", "docs", &["docs.edit"]),
    ("manage_doc_tab", "docs", &["docs.edit"]),
    // Sheets
    ("list_spreadsheets", "sheets", &["sheets.read"]),
    ("get_spreadsheet_info", "sheets", &["sheets.read"]),
    ("read_sheet_values", "sheets", &["sheets.read"]),
    ("list_sheet_tables", "sheets", &["sheets.read"]),
    ("create_spreadsheet", "sheets", &["sheets.create"]),
    ("modify_sheet_values", "sheets", &["sheets.edit"]),
    ("format_sheet_range", "sheets", &["sheets.edit"]),
    ("manage_conditional_formatting", "sheets", &["sheets.edit"]),
    ("create_sheet", "sheets", &["sheets.edit"]),
    ("append_table_rows", "sheets", &["sheets.edit"]),
    ("manage_sheet_tab", "sheets", &["sheets.edit"]),
    ("resize_sheet_dimensions", "sheets", &["sheets.edit"]),
    ("move_sheet_rows", "sheets", &["sheets.edit"]),
    (
        "manage_named_range",
        "sheets",
        &["sheets.read", "sheets.edit"],
    ),
    // Slides
    ("get_presentation", "slides", &["slides.read"]),
    ("get_page", "slides", &["slides.read"]),
    ("get_page_thumbnail", "slides", &["slides.read"]),
    ("create_presentation", "slides", &["slides.create"]),
    ("batch_update_presentation", "slides", &["slides.edit"]),
    // Forms
    ("get_form", "forms", &["forms.read"]),
    ("get_form_response", "forms", &["forms.responses"]),
    ("list_form_responses", "forms", &["forms.responses"]),
    ("create_form", "forms", &["forms.create"]),
    ("batch_update_form", "forms", &["forms.edit"]),
    ("set_publish_settings", "forms", &["forms.publish"]),
    // Tasks
    ("list_task_lists", "tasks", &["tasks.read"]),
    ("get_task_list", "tasks", &["tasks.read"]),
    ("list_tasks", "tasks", &["tasks.read"]),
    ("get_task", "tasks", &["tasks.read"]),
    ("manage_task", "tasks", &["tasks.edit", "tasks.delete"]),
    ("manage_task_list", "tasks", &["tasks.edit", "tasks.delete"]),
    // Contacts
    ("list_contacts", "contacts", &["contacts.read"]),
    ("get_contact", "contacts", &["contacts.read"]),
    ("search_contacts", "contacts", &["contacts.read"]),
    ("list_contact_groups", "contacts", &["contacts.read"]),
    ("get_contact_group", "contacts", &["contacts.read"]),
    (
        "manage_contact",
        "contacts",
        &["contacts.edit", "contacts.delete"],
    ),
    (
        "manage_contacts_batch",
        "contacts",
        &["contacts.edit", "contacts.delete"],
    ),
    (
        "manage_contact_group",
        "contacts",
        &["contacts.edit", "contacts.delete"],
    ),
];

fn tool(name: &str) -> Option<&'static (&'static str, &'static str, &'static [&'static str])> {
    TOOLS.iter().find(|(t, _, _)| *t == name)
}

/// The service a tool belongs to.
pub fn tool_service(name: &str) -> Option<&'static str> {
    tool(name).map(|(_, s, _)| *s)
}

/// Every action a tool can perform (empty for a tool Iris does not know).
pub fn tool_actions(name: &str) -> &'static [&'static str] {
    tool(name).map_or(&[], |(_, _, a)| a)
}

/// The value of a selector argument (`action`), as the server reads it
/// (Python's `str.strip()` and `str.lower()`; servers that compare exactly
/// refuse anything else).
fn selector(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|s| {
        s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
            .to_lowercase()
    })
}

/// The argument is given and not plainly empty.
fn given(args: &Value, key: &str) -> bool {
    match args.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(_) => true,
    }
}

/// The argument is given and not plainly false.
fn set_true(args: &Value, key: &str) -> bool {
    match args.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "" | "false" | "0" | "no" | "off"
        ),
        Some(_) => true,
    }
}

/// Any string inside `v` contains `needle` (case-insensitive).
fn mentions(v: Option<&Value>, needle: &str) -> bool {
    match v {
        Some(Value::String(s)) => s.to_ascii_lowercase().contains(needle),
        Some(Value::Array(a)) => a.iter().any(|x| mentions(Some(x), needle)),
        Some(Value::Object(o)) => o.values().any(|x| mentions(Some(x), needle)),
        _ => false,
    }
}

/// Any key inside `v` contains `needle` with a value given. The server
/// also accepts an object sent as a JSON string; text that is not JSON
/// counts when it mentions `needle` at all.
fn has_key(v: Option<&Value>, needle: &str) -> bool {
    match v {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(inner @ (Value::Object(_) | Value::Array(_))) => has_key(Some(&inner), needle),
            _ => s.to_ascii_lowercase().contains(needle),
        },
        Some(Value::Object(o)) => o.iter().any(|(k, x)| {
            (k.to_ascii_lowercase().contains(needle)
                && !matches!(x, Value::Null)
                && x != &Value::String(String::new()))
                || has_key(Some(x), needle)
        }),
        Some(Value::Array(a)) => a.iter().any(|x| has_key(Some(x), needle)),
        _ => false,
    }
}

/// What one call does: each action it performs, with the call's category
/// for that action. `None` when the tool or an argument value that chooses
/// what it does is unknown (the caller falls back to the general
/// classification).
pub fn call_actions(name: &str, args: &Value) -> Option<Vec<(&'static str, Category)>> {
    let (_, _, actions) = tool(name)?;
    let one = |id: &'static str, c: Category| Some(vec![(id, c)]);
    let act = || selector(args, "action");
    match name {
        "modify_gmail_message_labels" | "batch_modify_gmail_message_labels" => {
            if mentions(args.get("add_label_ids"), "trash") {
                one("gmail.trash", Delete)
            } else {
                one("gmail.organize", Write)
            }
        }
        "manage_gmail_label" => match act()?.as_str() {
            "create" | "update" => one("gmail.labels", Write),
            "delete" => one("gmail.labels", Delete),
            _ => None,
        },
        "manage_gmail_filter" => {
            let a = act()?;
            let forwards = has_key(args.get("filter_action"), "forward");
            match a.as_str() {
                "create" | "update" if forwards => Some(vec![
                    ("gmail.filters", Write),
                    ("gmail.forward_filters", Send),
                ]),
                "create" | "update" | "apply" => one("gmail.filters", Write),
                "delete" => one("gmail.filters", Delete),
                _ => None,
            }
        }
        "manage_event" => {
            let guests = given(args, "attendees");
            // Google tells guests about changes unless told not to.
            let silent = args.get("send_updates").and_then(Value::as_str) == Some("none");
            match act()?.as_str() {
                "create" if guests => one("calendar.events.invite", Send),
                "create" => one("calendar.events.create", Write),
                "update" if guests || !silent => one("calendar.events.notify", Send),
                "update" => one("calendar.events.update", Write),
                "delete" => one("calendar.events.delete", Delete),
                "rsvp" => one("calendar.events.rsvp", Send),
                _ => None,
            }
        }
        "manage_out_of_office" | "manage_focus_time" => match act()?.as_str() {
            "list" => one("calendar.events.read", Read),
            "create" | "update" => one("calendar.status", Send),
            "delete" => one("calendar.status", Delete),
            _ => None,
        },
        "update_drive_file" => {
            let mut out = Vec::new();
            if set_true(args, "trashed") {
                out.push(("drive.trash", Delete));
            }
            if given(args, "writers_can_share") || given(args, "copy_requires_writer_permission") {
                out.push(("drive.share", Send));
            }
            let other = args.as_object().is_some_and(|o| {
                o.keys().any(|k| {
                    ![
                        "file_id",
                        "user_google_email",
                        "trashed",
                        "writers_can_share",
                        "copy_requires_writer_permission",
                    ]
                    .contains(&k.as_str())
                })
            });
            if out.is_empty() || other {
                out.push(("drive.edit", Write));
            }
            Some(out)
        }
        "manage_drive_access" => match act()?.as_str() {
            "grant" | "grant_batch" | "update" | "transfer_owner" => one("drive.share", Send),
            "revoke" => one("drive.share", Delete),
            _ => None,
        },
        "manage_doc_tab" => match act()?.as_str() {
            "create" | "rename" | "populate_from_markdown" => one("docs.edit", Write),
            "delete" => one("docs.edit", Delete),
            _ => None,
        },
        "modify_sheet_values" if set_true(args, "clear_values") => one("sheets.edit", Delete),
        "manage_sheet_tab" => match act()?.as_str() {
            "rename" | "hide" | "unhide" | "reorder" => one("sheets.edit", Write),
            "delete" => one("sheets.edit", Delete),
            _ => None,
        },
        "manage_named_range" => match act()?.as_str() {
            "list" => one("sheets.read", Read),
            "create" | "update" => one("sheets.edit", Write),
            "delete" => one("sheets.edit", Delete),
            _ => None,
        },
        "manage_conditional_formatting" => match act()?.as_str() {
            "add" | "update" => one("sheets.edit", Write),
            "delete" => one("sheets.edit", Delete),
            _ => None,
        },
        "manage_task" => match act()?.as_str() {
            "create" | "update" | "move" => one("tasks.edit", Write),
            "delete" => one("tasks.delete", Delete),
            _ => None,
        },
        "manage_task_list" => match act()?.as_str() {
            "create" | "update" => one("tasks.edit", Write),
            "delete" | "clear_completed" => one("tasks.delete", Delete),
            _ => None,
        },
        "manage_contact" | "manage_contacts_batch" => match act()?.as_str() {
            "create" | "update" => one("contacts.edit", Write),
            "delete" => one("contacts.delete", Delete),
            _ => None,
        },
        "manage_contact_group" => match act()?.as_str() {
            "create" | "update" | "modify_members" => one("contacts.edit", Write),
            "delete" => one("contacts.delete", Delete),
            _ => None,
        },
        // Every other known tool does one thing.
        _ => {
            let id = actions[0];
            one(id, action(id)?.category)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every tool `workspace-mcp@2.1.0` offers for the services Iris turns
    /// on (`connectors::GOOGLE_TOOLS`), read from its source.
    const SERVER_TOOLS: &[&str] = &[
        "search_gmail_messages",
        "get_gmail_message_content",
        "get_gmail_messages_content_batch",
        "get_gmail_attachment_content",
        "send_gmail_message",
        "draft_gmail_message",
        "get_gmail_thread_content",
        "get_gmail_threads_content_batch",
        "list_gmail_labels",
        "manage_gmail_label",
        "list_gmail_filters",
        "manage_gmail_filter",
        "modify_gmail_message_labels",
        "batch_modify_gmail_message_labels",
        "list_calendars",
        "get_events",
        "manage_event",
        "manage_out_of_office",
        "manage_focus_time",
        "query_freebusy",
        "create_calendar",
        "search_drive_files",
        "get_drive_file_content",
        "get_drive_file_download_url",
        "list_drive_items",
        "create_drive_folder",
        "create_drive_file",
        "import_to_google_doc",
        "import_to_google_slides",
        "import_to_google_sheets",
        "get_drive_file_permissions",
        "check_drive_file_public_access",
        "update_drive_file",
        "get_drive_shareable_link",
        "manage_drive_access",
        "copy_drive_file",
        "set_drive_file_permissions",
        "search_docs",
        "get_doc_content",
        "list_docs_in_folder",
        "create_doc",
        "modify_doc_text",
        "find_and_replace_doc",
        "insert_doc_elements",
        "insert_doc_image",
        "update_doc_headers_footers",
        "batch_update_doc",
        "inspect_doc_structure",
        "debug_docs_runtime_info",
        "create_table_with_data",
        "debug_table_structure",
        "export_doc_to_pdf",
        "update_paragraph_style",
        "get_doc_as_markdown",
        "manage_doc_tab",
        "list_spreadsheets",
        "get_spreadsheet_info",
        "read_sheet_values",
        "modify_sheet_values",
        "format_sheet_range",
        "manage_conditional_formatting",
        "create_spreadsheet",
        "create_sheet",
        "list_sheet_tables",
        "append_table_rows",
        "manage_sheet_tab",
        "resize_sheet_dimensions",
        "move_sheet_rows",
        "manage_named_range",
        "create_presentation",
        "get_presentation",
        "batch_update_presentation",
        "get_page",
        "get_page_thumbnail",
        "create_form",
        "get_form",
        "set_publish_settings",
        "get_form_response",
        "list_form_responses",
        "batch_update_form",
        "list_task_lists",
        "get_task_list",
        "manage_task_list",
        "list_tasks",
        "get_task",
        "manage_task",
        "list_contacts",
        "get_contact",
        "search_contacts",
        "manage_contact",
        "list_contact_groups",
        "get_contact_group",
        "manage_contacts_batch",
        "manage_contact_group",
    ];

    #[test]
    fn every_server_tool_is_mapped_to_known_actions_of_its_service() {
        for t in SERVER_TOOLS {
            let svc = tool_service(t).unwrap_or_else(|| panic!("{t} is not mapped"));
            assert!(crate::connectors::GOOGLE_TOOLS.contains(&svc), "{t}: {svc}");
            let actions = tool_actions(t);
            assert!(!actions.is_empty(), "{t}");
            for id in actions {
                let a = action(id).unwrap_or_else(|| panic!("{t}: unknown action {id}"));
                let owner = service(svc).unwrap();
                assert!(owner.actions.iter().any(|x| x.id == a.id), "{t}: {id}");
            }
        }
        assert_eq!(TOOLS.len(), SERVER_TOOLS.len());
        // Action ids are unique and every action is reachable from a tool.
        let mut ids: Vec<&str> = SERVICES
            .iter()
            .flat_map(|s| s.actions)
            .map(|a| a.id)
            .collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
        for id in ids {
            assert!(
                TOOLS.iter().any(|(_, _, a)| a.contains(&id)),
                "{id} is never used"
            );
        }
        // Every service Iris turns on has a screen, plus YouTube.
        for s in crate::connectors::GOOGLE_TOOLS {
            assert!(service(s).is_some(), "{s}");
        }
        assert!(service("youtube").unwrap().actions.is_empty());
    }

    #[test]
    fn a_call_only_performs_actions_its_tool_declares() {
        let samples = [
            json!({}),
            json!({"action": "create"}),
            json!({"action": "create", "attendees": ["sam@example.com"]}),
            json!({"action": "update", "send_updates": "none"}),
            json!({"action": "update"}),
            json!({"action": "delete"}),
            json!({"action": "rsvp"}),
            json!({"action": "list"}),
            json!({"action": "apply"}),
            json!({"action": "grant"}),
            json!({"action": "revoke"}),
            json!({"action": "clear_completed"}),
            json!({"action": "modify_members"}),
            json!({"trashed": true, "writers_can_share": true, "name": "x"}),
            json!({"add_label_ids": ["TRASH"]}),
            json!({"action": "create", "filter_action": {"forward": "sam@example.com"}}),
        ];
        for t in SERVER_TOOLS {
            for args in &samples {
                if let Some(parts) = call_actions(t, args) {
                    assert!(!parts.is_empty(), "{t} {args}");
                    for (id, _) in parts {
                        assert!(tool_actions(t).contains(&id), "{t} {args} -> {id}");
                    }
                }
            }
        }
    }

    #[test]
    fn arguments_choose_the_action_and_unknown_values_fall_back() {
        let c = |t, args: Value| call_actions(t, &args);
        assert_eq!(
            c("send_gmail_message", json!({"to": "a@example.com"})),
            Some(vec![("gmail.send", Send)])
        );
        assert_eq!(
            c("search_gmail_messages", json!({"query": "x"})),
            Some(vec![("gmail.read", Read)])
        );
        assert_eq!(
            c("draft_gmail_message", json!({})),
            Some(vec![("gmail.draft", Draft)])
        );
        // Calendar: invitations and guest notices are their own actions.
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "create", "summary": "Lunch"})
            ),
            Some(vec![("calendar.events.create", Write)])
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": " Create ", "attendees": [{"email": "sam@example.com"}]})
            ),
            Some(vec![("calendar.events.invite", Send)])
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "update", "event_id": "e1"})
            ),
            Some(vec![("calendar.events.notify", Send)]),
            "Google notifies guests by default"
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "update", "event_id": "e1", "send_updates": "externalOnly"})
            ),
            Some(vec![("calendar.events.notify", Send)])
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "update", "event_id": "e1", "send_updates": "none"})
            ),
            Some(vec![("calendar.events.update", Write)])
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "update", "send_updates": "none", "attendees": ["x@example.com"]})
            ),
            Some(vec![("calendar.events.notify", Send)])
        );
        assert_eq!(
            c("manage_event", json!({"action": "delete"})),
            Some(vec![("calendar.events.delete", Delete)])
        );
        assert_eq!(
            c(
                "manage_event",
                json!({"action": "rsvp", "response": "accepted"})
            ),
            Some(vec![("calendar.events.rsvp", Send)])
        );
        // Unknown or missing selectors are not placed here.
        assert_eq!(c("manage_event", json!({"action": "frobnicate"})), None);
        assert_eq!(c("manage_event", json!({})), None);
        assert_eq!(c("manage_event", json!({"action": 3})), None);
        assert_eq!(c("unknown_tool", json!({})), None);
        // Drive: trashing and sharing settings are not plain edits.
        assert_eq!(
            c(
                "update_drive_file",
                json!({"file_id": "f", "trashed": true})
            ),
            Some(vec![("drive.trash", Delete)])
        );
        assert_eq!(
            c(
                "update_drive_file",
                json!({"file_id": "f", "trashed": "True"})
            ),
            Some(vec![("drive.trash", Delete)])
        );
        assert_eq!(
            c(
                "update_drive_file",
                json!({"file_id": "f", "trashed": false, "name": "n"})
            ),
            Some(vec![("drive.edit", Write)])
        );
        assert_eq!(
            c(
                "update_drive_file",
                json!({"file_id": "f", "writers_can_share": true, "name": "n"})
            ),
            Some(vec![("drive.share", Send), ("drive.edit", Write)])
        );
        assert_eq!(
            c(
                "manage_drive_access",
                json!({"action": "grant", "share_with": "sam@example.com"})
            ),
            Some(vec![("drive.share", Send)])
        );
        // Gmail: the trash label deletes; forwarding filters send.
        assert_eq!(
            c(
                "batch_modify_gmail_message_labels",
                json!({"message_ids": ["m"], "add_label_ids": "STARRED,trash"})
            ),
            Some(vec![("gmail.trash", Delete)])
        );
        assert_eq!(
            c(
                "modify_gmail_message_labels",
                json!({"message_id": "m", "remove_label_ids": ["INBOX"]})
            ),
            Some(vec![("gmail.organize", Write)])
        );
        assert_eq!(
            c(
                "manage_gmail_filter",
                json!({"action": "create", "criteria": {"from": "x@example.com"},
                       "filter_action": {"forward": "sam@example.com"}})
            ),
            Some(vec![
                ("gmail.filters", Write),
                ("gmail.forward_filters", Send)
            ])
        );
        assert_eq!(
            c(
                "manage_gmail_filter",
                json!({"action": "create", "filter_action": {"addLabelIds": ["L1"]}})
            ),
            Some(vec![("gmail.filters", Write)])
        );
        // The server accepts the object as a JSON string too.
        assert_eq!(
            c(
                "manage_gmail_filter",
                json!({"action": "update", "filter_action": "{\"forward\": \"sam@example.com\"}"})
            ),
            Some(vec![
                ("gmail.filters", Write),
                ("gmail.forward_filters", Send)
            ])
        );
        // Python's strip() also removes these separators.
        assert_eq!(
            c("manage_event", json!({"action": "\u{1c}DELETE\u{3000}"})),
            Some(vec![("calendar.events.delete", Delete)])
        );
        // Lists inside multi-purpose tools are reads.
        assert_eq!(
            c("manage_out_of_office", json!({"action": "list"})),
            Some(vec![("calendar.events.read", Read)])
        );
        assert_eq!(
            c("manage_named_range", json!({"action": "LIST"})),
            Some(vec![("sheets.read", Read)])
        );
        assert_eq!(
            c("manage_task_list", json!({"action": "clear_completed"})),
            Some(vec![("tasks.delete", Delete)])
        );
    }
}
