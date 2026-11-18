CREATE TABLE `edge` (
	`id` integer PRIMARY KEY NOT NULL,
	`src_id` integer NOT NULL,
	`dst_id` integer NOT NULL,
	`kind` text NOT NULL,
	`tier` text NOT NULL,
	`file_id` integer NOT NULL,
	`line` integer DEFAULT 0 NOT NULL,
	`local_name` text DEFAULT '' NOT NULL,
	`evidence` text,
	FOREIGN KEY (`src_id`) REFERENCES `node`(`id`) ON UPDATE no action ON DELETE cascade,
	FOREIGN KEY (`dst_id`) REFERENCES `node`(`id`) ON UPDATE no action ON DELETE cascade,
	FOREIGN KEY (`file_id`) REFERENCES `file`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE UNIQUE INDEX `edge_src_id_dst_id_kind_file_id_line_local_name_unique` ON `edge` (`src_id`,`dst_id`,`kind`,`file_id`,`line`,`local_name`);--> statement-breakpoint
CREATE INDEX `idx_edge_dst_kind` ON `edge` (`dst_id`,`kind`,`tier`);--> statement-breakpoint
CREATE INDEX `idx_edge_src_kind` ON `edge` (`src_id`,`kind`,`tier`);--> statement-breakpoint
CREATE INDEX `idx_edge_file_line` ON `edge` (`file_id`,`line`);--> statement-breakpoint
CREATE INDEX `idx_edge_kind` ON `edge` (`kind`);--> statement-breakpoint
CREATE TABLE `file` (
	`id` integer PRIMARY KEY NOT NULL,
	`path` text NOT NULL,
	`language` text NOT NULL,
	`sha256` text NOT NULL,
	`size` integer NOT NULL,
	`line_count` integer NOT NULL,
	`parse_status` text NOT NULL,
	`parse_error_ranges` text,
	`indexed_generation` text NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX `file_path_unique` ON `file` (`path`);--> statement-breakpoint
CREATE INDEX `idx_file_language` ON `file` (`language`);--> statement-breakpoint
CREATE TABLE `generation` (
	`id` text PRIMARY KEY NOT NULL,
	`seq` integer NOT NULL,
	`published_at` text NOT NULL,
	`base_commit` text,
	`worktree_dirty` integer DEFAULT 0 NOT NULL,
	`files_indexed` integer NOT NULL,
	`nodes` integer NOT NULL,
	`edges` integer NOT NULL,
	`duration_ms` integer NOT NULL,
	`mode` text NOT NULL,
	`status` text NOT NULL
);
--> statement-breakpoint
CREATE UNIQUE INDEX `generation_seq_unique` ON `generation` (`seq`);--> statement-breakpoint
CREATE TABLE `meta` (
	`key` text PRIMARY KEY NOT NULL,
	`value` text NOT NULL
);
--> statement-breakpoint
CREATE TABLE `node` (
	`id` integer PRIMARY KEY NOT NULL,
	`file_id` integer NOT NULL,
	`parent_id` integer,
	`kind` text NOT NULL,
	`subkind` text,
	`name` text NOT NULL,
	`qn` text NOT NULL,
	`start_line` integer NOT NULL,
	`end_line` integer NOT NULL,
	`signature` text,
	`docstring` text,
	`is_test` integer DEFAULT 0 NOT NULL,
	`props` text DEFAULT '{}' NOT NULL,
	FOREIGN KEY (`file_id`) REFERENCES `file`(`id`) ON UPDATE no action ON DELETE cascade,
	FOREIGN KEY (`parent_id`) REFERENCES `node`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE UNIQUE INDEX `node_qn_unique` ON `node` (`qn`);--> statement-breakpoint
CREATE INDEX `idx_node_name_kind` ON `node` (`name`,`kind`);--> statement-breakpoint
CREATE INDEX `idx_node_file_line` ON `node` (`file_id`,`start_line`);--> statement-breakpoint
CREATE INDEX `idx_node_parent` ON `node` (`parent_id`);--> statement-breakpoint
CREATE INDEX `idx_node_kind` ON `node` (`kind`);--> statement-breakpoint
CREATE TABLE `node_fts_state` (
	`rowid_` integer PRIMARY KEY NOT NULL,
	`name` text NOT NULL,
	`qn` text NOT NULL,
	`signature` text,
	`docstring` text,
	`props_text` text
);
--> statement-breakpoint
CREATE TABLE `node_metrics` (
	`node_id` integer PRIMARY KEY NOT NULL,
	`in_degree` integer DEFAULT 0 NOT NULL,
	`out_degree` integer DEFAULT 0 NOT NULL,
	`is_entry` integer DEFAULT 0 NOT NULL,
	`tested_depth` integer,
	`reachable_count` integer,
	FOREIGN KEY (`node_id`) REFERENCES `node`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE INDEX `idx_metrics_entry` ON `node_metrics` (`is_entry`);--> statement-breakpoint
CREATE INDEX `idx_metrics_tested` ON `node_metrics` (`tested_depth`);--> statement-breakpoint
CREATE TABLE `node_vector` (
	`node_id` integer PRIMARY KEY NOT NULL,
	`dim` integer NOT NULL,
	`vec` blob NOT NULL,
	FOREIGN KEY (`node_id`) REFERENCES `node`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE TABLE `not_indexed` (
	`id` integer PRIMARY KEY NOT NULL,
	`path` text NOT NULL,
	`reason` text NOT NULL,
	`detail` text,
	`language` text
);
--> statement-breakpoint
CREATE UNIQUE INDEX `not_indexed_path_reason_unique` ON `not_indexed` (`path`,`reason`);--> statement-breakpoint
CREATE INDEX `idx_not_indexed_path` ON `not_indexed` (`path`);--> statement-breakpoint
CREATE TABLE `tombstone` (
	`qn` text PRIMARY KEY NOT NULL,
	`kind` text NOT NULL,
	`file_id` integer,
	`last_seen_generation` text NOT NULL,
	`removed_in_generation` text NOT NULL,
	`had_callers` integer DEFAULT 0 NOT NULL
);
--> statement-breakpoint
CREATE TABLE `unresolved` (
	`id` integer PRIMARY KEY NOT NULL,
	`file_id` integer NOT NULL,
	`line` integer NOT NULL,
	`raw_target` text NOT NULL,
	`evidence_kind` text,
	`evidence` text,
	FOREIGN KEY (`file_id`) REFERENCES `file`(`id`) ON UPDATE no action ON DELETE cascade
);
--> statement-breakpoint
CREATE UNIQUE INDEX `unresolved_file_id_line_raw_target_unique` ON `unresolved` (`file_id`,`line`,`raw_target`);--> statement-breakpoint
CREATE INDEX `idx_unresolved_file` ON `unresolved` (`file_id`);