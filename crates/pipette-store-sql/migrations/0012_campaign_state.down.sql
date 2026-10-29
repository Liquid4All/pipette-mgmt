-- Every campaign reads as active again: the archived marks are lost.
ALTER TABLE campaigns DROP COLUMN state;
