function requiredEnv(key: string, opts?: { allowed?: string[]; pattern?: RegExp; minLength?: number; maxLength?: number }): string {
  const value = process.env[key];
  if (value === undefined || value === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  if (opts?.allowed && !opts.allowed.includes(value)) {
    throw new Error(`${key} must be one of: ${opts.allowed.join(", ")}`);
  }
  if (opts?.pattern && !opts.pattern.test(value)) {
    throw new Error(`${key} does not match pattern: ${opts.pattern}`);
  }
  if (opts?.minLength !== undefined && value.length < opts.minLength) {
    throw new Error(`${key} must be at least ${opts.minLength} characters`);
  }
  if (opts?.maxLength !== undefined && value.length > opts.maxLength) {
    throw new Error(`${key} must be at most ${opts.maxLength} characters`);
  }
  return value;
}
