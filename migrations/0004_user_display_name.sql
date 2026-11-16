-- A user's full name, as it appears in companies' account owner lists ("Cheryl Smith").
-- Set by AR admins on Users & access; links a sign-in to the accounts that person owns (My Work page).
ALTER TABLE users ADD COLUMN display_name TEXT;
