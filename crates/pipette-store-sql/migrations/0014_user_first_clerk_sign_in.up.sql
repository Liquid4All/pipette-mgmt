-- An account's first Clerk sign-in and the keys it held then. The primary key
-- is the exclusive create that lets one first sign-in win.
CREATE TABLE user_first_clerk_sign_in (
    user_id  text PRIMARY KEY,
    body     text NOT NULL
);
