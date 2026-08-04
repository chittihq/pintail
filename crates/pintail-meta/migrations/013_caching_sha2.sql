ALTER TABLE api_keys
ADD COLUMN caching_sha2_password_hash BLOB;

PRAGMA user_version = 13;
