
## Crash recovery alongside startup Help

Exact-head CI exposed that the runtime crash fixture's original zero-publication premise no
longer holds once the runtime owns startup Help generation. Help legitimately creates its own
receipt and files. The fixture now settles actual Help/receipt startup before arming its file
crash injection and captures that independent baseline. All original exactly-once queue,
receipt/artifact/event/file, no-orphan, recovery classification and next-boot assertions remain,
measuring exactly one additional user receipt across the same two crash points. No timeout
increased, consumer disabled or product behavior changed. All three native runtime cases pass
in 16.09 s; earlier hosted Linux/Windows failures both showed two legitimate completed receipts.
