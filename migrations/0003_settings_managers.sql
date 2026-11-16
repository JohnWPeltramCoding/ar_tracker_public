-- Company settings are no longer open to every assigned user: only AR admins and users an
-- admin has made a "settings manager" for that company (requires access to the company).
ALTER TABLE user_companies ADD COLUMN can_manage_settings BOOLEAN NOT NULL DEFAULT false;
