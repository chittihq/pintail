ALTER TABLE api_keys
ADD COLUMN mysql_native_password_hash BLOB;

PRAGMA user_version = 6;
