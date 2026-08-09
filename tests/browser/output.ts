export function redactBootSecrets(output: string): string {
  return output.replace(
    /^(PINTAIL_(?:DSN_ENCRYPTION_KEY|JWT_SECRET)=).*$/gm,
    '$1<redacted>',
  )
}
