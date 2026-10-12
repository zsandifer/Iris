-- Browser migration shipped on main; apply after main-schema-v4.sql.

-- Browser wallet approvals share the gateway cards and cancellation states.
CREATE INDEX messages_pending_approvals ON messages(conversation_id, created_at)
    WHERE kind = 'card' AND json_extract(card, '$.type') = 'approval'
      AND json_extract(card, '$.state') = 'pending';

PRAGMA user_version = 5;
