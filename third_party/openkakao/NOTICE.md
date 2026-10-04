Notification Center read-only query and payload field conventions were adapted from JungHoonGhae/openkakao-cli src/commands/notif_watch.rs (MIT).

Chat-row AX selection and verification conventions in native/kakao/ReplySender.swift were adapted from its src/ax_send.rs (MIT). The sender is a separate owner-authorized application, not an invocation of OpenKakao with its authentication checks bypassed.

Source: https://github.com/JungHoonGhae/openkakao-cli

No server login, credential extraction, or encrypted chat-database decryption is included. The upstream license is reproduced in LICENSE.
