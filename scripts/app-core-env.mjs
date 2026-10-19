import assert from 'node:assert/strict';
import {accessSync, constants} from 'node:fs';
import path from 'node:path';

// Native credentials stay in the explicitly selected account home; never copy or
// discover legacy ACP credentials. Catalog-only checks also need this account.
export function appCoreEnv() {
  for (const name of ['AGY_APP_CORE_BINARY', 'AGY_APP_AUTH_HOME']) {
    assert.ok(process.env[name], `${name} is required; legacy ACP credentials and binaries are not discovered`);
    assert.ok(path.isAbsolute(process.env[name]), `${name} must be an absolute path`);
  }
  accessSync(process.env.AGY_APP_CORE_BINARY, constants.X_OK);
  accessSync(path.join(process.env.AGY_APP_AUTH_HOME, '.gemini', 'jetski-standalone-oauth-token'), constants.R_OK);
  const env = {...process.env};
  for (const name of ['AGY_EXTERNAL_BINARY', 'AGY_PERSONAL_CREDENTIALS', 'AGY_API_BASE_URL', 'GEMINI_API_KEY', 'GOOGLE_API_KEY', 'GOOGLE_APPLICATION_CREDENTIALS', 'AGY_APP_SERVER_HOME', 'AGY_HARNESS_HOME']) delete env[name];
  return env;
}
