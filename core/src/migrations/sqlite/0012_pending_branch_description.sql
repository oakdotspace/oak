ALTER TABLE branches ADD COLUMN description_pending INTEGER NOT NULL DEFAULT 0;
-- Existing descriptions have no recorded synchronization baseline. Preserve
-- them until a matching remote observation/publication establishes one.
UPDATE branches SET description_pending = 1 WHERE description IS NOT NULL;
