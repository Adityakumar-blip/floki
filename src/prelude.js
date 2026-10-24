// Floki script API, evaluated before user scripts. `__ctx` is injected by script.rs; results go to `__out`.
"use strict";
const __out = { tests: [], logs: [], writes: [], errors: [], next: undefined, skip: false, request: null };

const __show = (v) => {
  if (typeof v === "string") return JSON.stringify(v);
  if (v === undefined) return "undefined";
  if (typeof v === "function") return "[function]";
  try {
    return JSON.stringify(v);
  } catch (e) {
    return String(v);
  }
};
const __text = (v) => (typeof v === "string" ? v : __show(v));

const console = {};
for (const level of ["log", "info", "warn", "error", "debug"]) {
  console[level] = (...args) => __out.logs.push({ level, text: args.map(__text).join(" ") });
}

const __string = (v) => (v == null ? "" : typeof v === "string" ? v : typeof v === "object" ? JSON.stringify(v) : String(v));

function __scope(name, store, readOnly) {
  const scope = {
    get: (key) => store[key],
    has: (key) => Object.prototype.hasOwnProperty.call(store, key),
    toObject: () => Object.assign({}, store),
    replaceIn: (s) => __replace(s),
  };
  if (!readOnly) {
    scope.set = (key, value) => {
      key = String(key);
      store[key] = __string(value);
      __out.writes.push([name, key, store[key]]);
    };
    scope.unset = (key) => {
      key = String(key);
      delete store[key];
      __out.writes.push([name, key, null]);
    };
    scope.clear = () => Object.keys(store).forEach(scope.unset);
  }
  return scope;
}

const __stores = {
  local: Object.assign({}, __ctx.local),
  data: Object.assign({}, __ctx.data),
  environment: Object.assign({}, __ctx.environment),
  collection: Object.assign({}, __ctx.collection),
  globals: Object.assign({}, __ctx.globals),
};
const env = __scope("environment", __stores.environment);
if (!__ctx.hasEnvironment) {
  const set = env.set;
  env.set = (key, value) => {
    console.warn(`No environment is selected, so env.set("${key}") only lasts for this request`);
    set(key, value);
  };
}
const globals = __scope("globals", __stores.globals);
const collection = __scope("collection", __stores.collection);
const data = __scope("data", __stores.data, true);

/** Resolves a name the way {{name}} does: local, data, environment, collection, then globals. */
const vars = __scope("local", __stores.local);
vars.get = (key) => {
  for (const s of ["local", "data", "environment", "collection", "globals"]) {
    if (Object.prototype.hasOwnProperty.call(__stores[s], key)) return __stores[s][key];
  }
  return undefined;
};
vars.has = (key) => vars.get(key) !== undefined;
vars.toObject = () => Object.assign({}, __stores.globals, __stores.collection, __stores.environment, __stores.data, __stores.local);

function __replace(s) {
  return String(s).replace(/\{\{([^{}]+)\}\}/g, (match, key) => {
    const value = vars.get(key.trim());
    return value === undefined ? match : value;
  });
}

function __headers(list) {
  const find = (name) => list.findIndex(([k]) => k.toLowerCase() === String(name).toLowerCase());
  return {
    get: (name) => (find(name) < 0 ? undefined : list[find(name)][1]),
    has: (name, value) => find(name) >= 0 && (value === undefined || list[find(name)][1] === value),
    all: () => list.map(([key, value]) => ({ key, value })),
    toObject: () => Object.fromEntries(list.map(([k, v]) => [k.toLowerCase(), v])),
    set(name, value) {
      this.remove(name);
      list.push([String(name), __string(value)]);
    },
    add(name, value) {
      list.push([String(name), __string(value)]);
    },
    remove(name) {
      for (let i = find(name); i >= 0; i = find(name)) list.splice(i, 1);
    },
    list,
  };
}

const req = {
  name: __ctx.request.name,
  method: __ctx.request.method,
  url: __ctx.request.url,
  body: __ctx.request.body,
  headers: __headers(__ctx.request.headers.map((h) => h.slice())),
};

function __cookies(list) {
  const find = (name) => list.find((c) => c.name === name);
  return {
    get: (name) => (find(name) || {}).value,
    has: (name) => find(name) !== undefined,
    all: () => list.map((c) => Object.assign({}, c)),
    toObject: () => Object.fromEntries(list.map((c) => [c.name, c.value])),
    jar() {
      throw new Error("The cookie jar is not scriptable yet; use the Cookies manager");
    },
  };
}

function __response(r) {
  let parsed;
  return {
    status: r.status,
    statusText: r.reason,
    time: r.time,
    size: r.size,
    body: r.body,
    headers: __headers(r.headers),
    cookies: __cookies(r.cookies || []),
    /** Server-sent events, for text/event-stream responses. */
    events: (r.events || []).map((e) => Object.assign({ json: () => JSON.parse(e.data) }, e)),
    /** The streamed answer of an OpenAI, Anthropic or Gemini style stream, else null. */
    assembled: r.assembled === undefined ? null : r.assembled,
    text: () => r.body,
    json() {
      if (parsed === undefined) {
        try {
          parsed = JSON.parse(r.body);
        } catch (e) {
          throw new Error("Response body is not valid JSON");
        }
      }
      return parsed;
    },
  };
}

/** Postman's response shape, for pm.response and pm.sendRequest callbacks. */
function __pmResponse(r) {
  return {
    code: r.status,
    status: r.statusText,
    responseTime: r.time,
    responseSize: r.size,
    headers: r.headers,
    cookies: r.cookies,
    json: () => r.json(),
    text: () => r.text(),
    get to() {
      return __responseChain(false, r).to;
    },
  };
}

const res = __ctx.response ? __response(__ctx.response) : undefined;

const __message = (e) => (e && e.message !== undefined ? e.message : String(e));

/** pm.sendRequest(urlOrRequest, callback?): sends now, then calls back and/or resolves the returned promise. */
function __sendRequest(spec, callback) {
  const r = typeof spec === "string" ? { url: spec } : spec || {};
  let headers = [];
  if (Array.isArray(r.header)) headers = r.header.filter((h) => !h.disabled).map((h) => [String(h.key), __string(h.value)]);
  else if (r.header && typeof r.header === "object") headers = Object.entries(r.header).map(([k, v]) => [k, __string(v)]);
  const url = r.url && typeof r.url === "object" ? r.url.raw || String(r.url) : String(r.url || "");
  let body = "";
  const mode = r.body && r.body.mode;
  const hasType = () => headers.some(([k]) => k.toLowerCase() === "content-type");
  if (mode === "raw") body = __string(r.body.raw);
  else if (mode === "urlencoded") {
    body = (r.body.urlencoded || []).filter((p) => !p.disabled).map((p) => `${encodeURIComponent(p.key)}=${encodeURIComponent(__replace(p.value))}`).join("&");
    if (!hasType()) headers.push(["Content-Type", "application/x-www-form-urlencoded"]);
  } else if (mode === "graphql") {
    body = JSON.stringify(r.body.graphql || {});
    if (!hasType()) headers.push(["Content-Type", "application/json"]);
  }
  const out = JSON.parse(
    __native_send(
      JSON.stringify({ method: (r.method || "GET").toUpperCase(), url: __replace(url), headers: headers.map(([k, v]) => [__replace(k), __replace(v)]), body: __replace(body) })
    )
  );
  const error = out.error ? new Error(out.error) : null;
  const response = error ? null : __pmResponse(__response(out.response));
  if (typeof callback === "function") {
    callback(error, response);
    return undefined;
  }
  return error ? Promise.reject(error) : Promise.resolve(response);
}

// Timers run after the script, in order of their delay; no real time passes.
const __timers = [];
let __clock = 0;
let __timerSeq = 0;
function setTimeout(fn, ms, ...args) {
  const id = ++__timerSeq;
  __timers.push({ id, at: __clock + (Number(ms) || 0), fn, args });
  return id;
}
function clearTimeout(id) {
  const i = __timers.findIndex((t) => t.id === id);
  if (i >= 0) __timers.splice(i, 1);
}
function __nextTimer() {
  if (!__timers.length) return false;
  __timers.sort((a, b) => a.at - b.at || a.id - b.id);
  const timer = __timers.shift();
  __clock = timer.at;
  try {
    timer.fn(...timer.args);
  } catch (e) {
    __out.errors.push(`setTimeout callback: ${__message(e)}`);
  }
  return true;
}

function __utf8Hex(s) {
  let hex = "";
  for (const ch of String(s)) {
    let c = ch.codePointAt(0);
    const bytes =
      c < 0x80 ? [c] : c < 0x800 ? [0xc0 | (c >> 6), 0x80 | (c & 63)] : c < 0x10000 ? [0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63)] : [0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63)];
    for (const b of bytes) hex += b.toString(16).padStart(2, "0");
  }
  return hex;
}
const __hexBytes = (hex) => (String(hex).match(/../g) || []).map((h) => parseInt(h, 16));
function __hexUtf8(hex) {
  const b = __hexBytes(hex);
  let out = "";
  for (let i = 0; i < b.length; ) {
    const c = b[i];
    const n = c < 0x80 ? 1 : c < 0xe0 ? 2 : c < 0xf0 ? 3 : 4;
    let cp = n === 1 ? c : c & (0xff >> (n + 1));
    for (let j = 1; j < n; j++) cp = (cp << 6) | (b[i + j] & 63);
    out += String.fromCodePoint(cp);
    i += n;
  }
  return out;
}
const __latin1Hex = (s) => Array.from(String(s), (c) => (c.charCodeAt(0) & 255).toString(16).padStart(2, "0")).join("");
const __hexLatin1 = (hex) => String.fromCharCode(...__hexBytes(hex));

/** The parts of crypto-js Postman scripts use for signing: hashes, HMACs and encoders. */
function WordArray(hex) {
  this.hex = hex;
  this.sigBytes = hex.length / 2;
}
WordArray.prototype.toString = function (encoder) {
  return (encoder || CryptoJS.enc.Hex).stringify(this);
};
WordArray.prototype.concat = function (other) {
  this.hex += other.hex;
  this.sigBytes = this.hex.length / 2;
  return this;
};
const __words = (v) => (v instanceof WordArray ? v : new WordArray(__utf8Hex(v == null ? "" : String(v))));
const CryptoJS = {
  enc: {
    Hex: { stringify: (w) => w.hex, parse: (s) => new WordArray(String(s).toLowerCase()) },
    Utf8: { stringify: (w) => __hexUtf8(w.hex), parse: (s) => new WordArray(__utf8Hex(s)) },
    Latin1: { stringify: (w) => __hexLatin1(w.hex), parse: (s) => new WordArray(__latin1Hex(s)) },
    Base64: { stringify: (w) => btoa(__hexLatin1(w.hex)), parse: (s) => new WordArray(__latin1Hex(atob(s))) },
  },
  lib: { WordArray: { create: () => new WordArray("") } },
};
for (const alg of ["MD5", "SHA1", "SHA224", "SHA256", "SHA384", "SHA512"]) {
  CryptoJS[alg] = (message) => new WordArray(__native_crypto("hash", alg, __words(message).hex, ""));
  CryptoJS[`Hmac${alg}`] = (message, key) => new WordArray(__native_crypto("hmac", alg, __words(message).hex, __words(key).hex));
}

function __uuid() {
  const hex = Array.from({ length: 32 }, () => Math.floor(Math.random() * 16).toString(16));
  hex[12] = "4";
  hex[16] = ((parseInt(hex[16], 16) & 3) | 8).toString(16);
  const h = hex.join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}


class AssertionError extends Error {}

function __equal(a, b) {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a);
  const kb = Object.keys(b);
  return ka.length === kb.length && ka.every((k) => Object.prototype.hasOwnProperty.call(b, k) && __equal(a[k], b[k]));
}

function __contains(haystack, needle, deep) {
  if (typeof haystack === "string") return haystack.includes(String(needle));
  if (Array.isArray(haystack)) return haystack.some((v) => (deep ? __equal(v, needle) : v === needle));
  if (haystack && typeof haystack === "object" && needle && typeof needle === "object") {
    return Object.keys(needle).every((k) => __equal(haystack[k], needle[k]));
  }
  return false;
}

function __path(obj, path) {
  const parts = Array.isArray(path) ? path : String(path).split(".");
  let cur = obj;
  for (const part of parts) {
    if (cur == null || !Object.prototype.hasOwnProperty.call(Object(cur), part)) return { found: false };
    cur = cur[part];
  }
  return { found: true, value: cur };
}

function __type(v) {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  return typeof v;
}

const __empty = (v) =>
  v == null || (typeof v === "string" || Array.isArray(v) ? v.length === 0 : typeof v === "object" && Object.keys(v).length === 0);

/** Jest-style: expect(value).toBe(1), expect(value).not.toContain("x"). */
function expect(actual) {
  const build = (negate) => {
    const check = (pass, description) => {
      if (Boolean(pass) === negate) throw new AssertionError(`expected ${__show(actual)} ${negate ? "not " : ""}${description}`);
    };
    return {
      toBe: (e) => check(Object.is(actual, e), `to be ${__show(e)}`),
      toEqual: (e) => check(__equal(actual, e), `to equal ${__show(e)}`),
      toBeTruthy: () => check(actual, "to be truthy"),
      toBeFalsy: () => check(!actual, "to be falsy"),
      toBeNull: () => check(actual === null, "to be null"),
      toBeUndefined: () => check(actual === undefined, "to be undefined"),
      toBeDefined: () => check(actual !== undefined, "to be defined"),
      toBeGreaterThan: (n) => check(actual > n, `to be greater than ${n}`),
      toBeGreaterThanOrEqual: (n) => check(actual >= n, `to be at least ${n}`),
      toBeLessThan: (n) => check(actual < n, `to be less than ${n}`),
      toBeLessThanOrEqual: (n) => check(actual <= n, `to be at most ${n}`),
      toContain: (x) => check(__contains(actual, x, false), `to contain ${__show(x)}`),
      toContainEqual: (x) => check(__contains(actual, x, true), `to contain ${__show(x)}`),
      toMatch: (re) => check(typeof actual === "string" && (re instanceof RegExp ? re.test(actual) : actual.includes(re)), `to match ${re}`),
      toHaveLength: (n) => check(actual != null && actual.length === n, `to have length ${n}`),
      toHaveProperty(path, value) {
        const hit = __path(actual, path);
        if (arguments.length > 1) check(hit.found && __equal(hit.value, value), `to have property "${path}" equal to ${__show(value)}`);
        else check(hit.found, `to have property "${path}"`);
      },
      toBeOneOf: (list) => check(list.some((v) => __equal(v, actual)), `to be one of ${__show(list)}`),
      toBeType: (type) => check(__type(actual) === type, `to be of type ${type}`),
    };
  };
  const matchers = build(false);
  matchers.not = build(true);
  return matchers;
}

/** Chai-style chain used by pm.expect: pm.expect(x).to.be.above(1), .to.have.property("a").that.equals(2). */
function __chai(actual, negate, deep) {
  const a = {};
  const check = (pass, description) => {
    if (Boolean(pass) === Boolean(negate)) throw new AssertionError(`expected ${__show(actual)} ${negate ? "not " : ""}${description}`);
    return a;
  };
  for (const word of ["to", "be", "been", "is", "that", "which", "and", "has", "have", "with", "at", "of", "same", "but", "does", "still", "also"]) {
    Object.defineProperty(a, word, { get: () => a });
  }
  Object.defineProperty(a, "not", { get: () => __chai(actual, !negate, deep) });
  Object.defineProperty(a, "deep", { get: () => __chai(actual, negate, true) });
  const getters = {
    ok: () => check(actual, "to be truthy"),
    true: () => check(actual === true, "to be true"),
    false: () => check(actual === false, "to be false"),
    null: () => check(actual === null, "to be null"),
    undefined: () => check(actual === undefined, "to be undefined"),
    NaN: () => check(Number.isNaN(actual), "to be NaN"),
    exist: () => check(actual != null, "to exist"),
    empty: () => check(__empty(actual), "to be empty"),
  };
  for (const [name, fn] of Object.entries(getters)) Object.defineProperty(a, name, { get: fn });
  const equal = (e) => check(deep ? __equal(actual, e) : actual === e, `to ${deep ? "deeply " : ""}equal ${__show(e)}`);
  const eql = (e) => check(__equal(actual, e), `to deeply equal ${__show(e)}`);
  const above = (n) => check(actual > n, `to be above ${n}`);
  const below = (n) => check(actual < n, `to be below ${n}`);
  const least = (n) => check(actual >= n, `to be at least ${n}`);
  const most = (n) => check(actual <= n, `to be at most ${n}`);
  const include = (x) => check(__contains(actual, x, deep), `to include ${__show(x)}`);
  const length = (n) => check(actual != null && actual.length === n, `to have length ${n}`);
  const type = (t) => check(__type(actual) === String(t).toLowerCase(), `to be a ${t}`);
  Object.assign(a, {
    equal, equals: equal, eq: equal,
    eql, eqls: eql,
    above, gt: above, greaterThan: above,
    below, lt: below, lessThan: below,
    least, gte: least,
    most, lte: most,
    within: (lo, hi) => check(actual >= lo && actual <= hi, `to be within ${lo}..${hi}`),
    include, includes: include, contain: include, contains: include,
    lengthOf: length,
    a: type, an: type,
    match: (re) => check(re.test(String(actual)), `to match ${re}`),
    string: (s) => check(typeof actual === "string" && actual.includes(s), `to contain ${__show(s)}`),
    oneOf: (list) => check(list.some((v) => (deep ? __equal(v, actual) : v === actual)), `to be one of ${__show(list)}`),
    instanceOf: (C) => check(actual instanceof C, `to be an instance of ${C.name}`),
    satisfy: (fn) => check(fn(actual), "to satisfy the given condition"),
    closeTo: (n, delta) => check(Math.abs(actual - n) <= delta, `to be close to ${n} ±${delta}`),
    keys(...keys) {
      if (keys.length === 1 && Array.isArray(keys[0])) keys = keys[0];
      return check(actual != null && keys.every((k) => Object.prototype.hasOwnProperty.call(Object(actual), k)), `to have keys ${__show(keys)}`);
    },
    members: (list) => check(Array.isArray(actual) && actual.length === list.length && list.every((v) => __contains(actual, v, true)), `to have members ${__show(list)}`),
    property(path, value) {
      const hit = __path(actual, path);
      if (arguments.length > 1) check(hit.found && __equal(hit.value, value), `to have property "${path}" equal to ${__show(value)}`);
      else check(hit.found, `to have property "${path}"`);
      return negate ? a : __chai(hit.value, false, deep);
    },
  });
  // `length` is a reserved function property, so it is assigned explicitly.
  Object.defineProperty(a, "length", { value: length });
  return a;
}

// pm.response.to.have.status(200), pm.response.to.be.ok
function __responseChain(negate, res) {
  const c = {};
  const check = (pass, description) => {
    if (Boolean(pass) === Boolean(negate)) throw new AssertionError(`expected response ${negate ? "not " : ""}${description}`);
    return c;
  };
  for (const word of ["to", "be", "have", "and", "with"]) Object.defineProperty(c, word, { get: () => c });
  Object.defineProperty(c, "not", { get: () => __responseChain(!negate, res) });
  const range = (lo, hi, label) => () => check(res.status >= lo && res.status <= hi, `to be ${label} but got ${res.status}`);
  for (const [name, fn] of Object.entries({
    ok: range(200, 299, "2xx"),
    success: range(200, 299, "2xx"),
    info: range(100, 199, "1xx"),
    redirection: range(300, 399, "3xx"),
    error: range(400, 599, "4xx or 5xx"),
    clientError: range(400, 499, "4xx"),
    serverError: range(500, 599, "5xx"),
    json: () => check((() => { try { res.json(); return true; } catch (e) { return false; } })(), "to have a JSON body"),
  })) {
    Object.defineProperty(c, name, { get: fn });
  }
  c.status = (s) =>
    typeof s === "number"
      ? check(res.status === s, `to have status ${s} but got ${res.status}`)
      : check(res.statusText === s, `to have status "${s}" but got "${res.statusText}"`);
  c.header = (name, value) =>
    check(res.headers.has(name, value), value === undefined ? `to have header "${name}"` : `to have header "${name}: ${value}"`);
  c.body = (text) => check(text === undefined ? res.body.length > 0 : res.body === text, text === undefined ? "to have a body" : `to have body ${__show(text)}`);
  c.jsonBody = (path, value) => {
    const hit = path === undefined ? { found: true } : __path(res.json(), path);
    return check(hit.found && (value === undefined || __equal(hit.value, value)), `to have JSON body ${path === undefined ? "" : `with "${path}"`}`);
  };
  c.responseTime = { below: (ms) => check(res.time < ms, `time ${res.time}ms to be below ${ms}ms`) };
  return c;
}

function test(name, fn) {
  const entry = { name: String(name), passed: true, error: null };
  __out.tests.push(entry);
  try {
    const result = fn();
    if (result && typeof result.then === "function") {
      Object.assign(entry, { passed: false, error: "Test did not finish" });
      result.then(
        () => Object.assign(entry, { passed: true, error: null }),
        (e) => Object.assign(entry, { passed: false, error: __message(e) })
      );
    }
  } catch (e) {
    Object.assign(entry, { passed: false, error: __message(e) });
  }
}
test.skip = (name) => __out.tests.push({ name: String(name), passed: true, skipped: true, error: null });

const info = { requestName: __ctx.request.name, iteration: __ctx.iteration, iterationCount: __ctx.iterationCount, eventName: __ctx.phase };
const setNextRequest = (name) => {
  __out.next = name === null ? null : String(name);
};
const skipRequest = () => {
  __out.skip = true;
};

// Postman compatibility: the pm.* names imported collections use, mapped onto the API above.

const pm = {
  test,
  expect: (v) => __chai(v, false, false),
  environment: env,
  globals,
  collectionVariables: collection,
  variables: vars,
  iterationData: data,
  info,
  execution: { setNextRequest, skipRequest },
  sendRequest: __sendRequest,
  cookies: res ? res.cookies : __cookies([]),
  request: {
    get method() {
      return req.method;
    },
    set method(m) {
      req.method = String(m);
    },
    url: {
      toString: () => req.url,
      update: (u) => {
        req.url = String(u);
      },
    },
    headers: {
      get: (k) => req.headers.get(k),
      has: (k) => req.headers.has(k),
      add: (h) => req.headers.add(h.key, h.value),
      upsert: (h) => req.headers.set(h.key, h.value),
      remove: (k) => req.headers.remove(k),
      toObject: () => req.headers.toObject(),
    },
    body: {
      mode: "raw",
      get raw() {
        return req.body;
      },
      set raw(v) {
        req.body = __string(v);
      },
      toString: () => req.body,
      update: (v) => {
        req.body = typeof v === "object" && v !== null && "raw" in v ? __string(v.raw) : __string(v);
      },
    },
  },
  response: res && __pmResponse(res),
};

const postman = {
  setEnvironmentVariable: (k, v) => env.set(k, v),
  getEnvironmentVariable: (k) => env.get(k),
  clearEnvironmentVariable: (k) => env.unset(k),
  setGlobalVariable: (k, v) => globals.set(k, v),
  getGlobalVariable: (k) => globals.get(k),
  clearGlobalVariable: (k) => globals.unset(k),
  setNextRequest,
};
const responseBody = res ? res.body : undefined;
const responseCode = res ? { code: res.status, name: res.statusText } : undefined;
const responseTime = res ? res.time : undefined;
const responseHeaders = res ? res.headers.toObject() : undefined;
const iteration = __ctx.iteration;

function require(name) {
  const modules = {
    "crypto-js": CryptoJS,
    uuid: { v4: __uuid },
    chai: { expect: (v) => __chai(v, false, false) },
    atob,
    btoa,
  };
  if (Object.prototype.hasOwnProperty.call(modules, name)) return modules[name];
  throw new Error(`require("${name}") is not available in Floki scripts; available: ${Object.keys(modules).join(", ")}`);
}

const __b64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
function btoa(s) {
  s = String(s);
  let out = "";
  for (let i = 0; i < s.length; i += 3) {
    const [a, b, c] = [s.charCodeAt(i), s.charCodeAt(i + 1), s.charCodeAt(i + 2)];
    if (a > 255 || b > 255 || c > 255) throw new Error("btoa: string contains characters outside Latin-1");
    const n = (a << 16) | ((b || 0) << 8) | (c || 0);
    out += __b64[(n >> 18) & 63] + __b64[(n >> 12) & 63] + (i + 1 < s.length ? __b64[(n >> 6) & 63] : "=") + (i + 2 < s.length ? __b64[n & 63] : "=");
  }
  return out;
}
function atob(s) {
  s = String(s).replace(/[\s=]/g, "");
  let out = "";
  for (let i = 0; i < s.length; i += 4) {
    const n = [0, 1, 2, 3].reduce((acc, j) => (acc << 6) | Math.max(0, __b64.indexOf(s[i + j] || "A")), 0);
    out += String.fromCharCode((n >> 16) & 255);
    if (i + 2 < s.length) out += String.fromCharCode((n >> 8) & 255);
    if (i + 3 < s.length) out += String.fromCharCode(n & 255);
  }
  return out;
}

const __AsyncFunction = async function () {}.constructor;

/** Starts one user script as an async function, so top-level await works; a throw stops that script only. */
function __run(label, source) {
  const tests = {};
  const legacy = () => {
    for (const [name, passed] of Object.entries(tests)) {
      __out.tests.push({ name, passed: Boolean(passed), error: passed ? null : "Assertion failed" });
    }
  };
  let script;
  try {
    script = new __AsyncFunction("tests", source);
  } catch (e) {
    __out.errors.push(`${label}: ${__message(e)}`);
    return;
  }
  script(tests).then(legacy, (e) => {
    __out.errors.push(`${label}: ${__message(e)}`);
    legacy();
  });
}

function __finish() {
  if (__ctx.phase === "prerequest") {
    __out.request = { method: req.method, url: String(req.url), headers: req.headers.list, body: req.body };
  }
  return JSON.stringify(__out);
}
