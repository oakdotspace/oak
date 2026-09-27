CREATE TABLE branch_description_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation INTEGER NOT NULL CHECK (generation >= 0)
);
INSERT INTO branch_description_clock VALUES (1, 0);
ALTER TABLE branches ADD COLUMN description_generation INTEGER NOT NULL DEFAULT 0;
