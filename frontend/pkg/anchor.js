/**
 * A resolved anchor, as seen from JS.
 *
 * A struct rather than a bare index because the caller needs to know *how* the
 * match was found — see [`How::Fallback`]. Returned as `Resolved | undefined`,
 * so a missing quote reads as falsy on the JS side.
 */
export class Resolved {
    static __wrap(ptr) {
        const obj = Object.create(Resolved.prototype);
        obj.__wbg_ptr = ptr;
        ResolvedFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        ResolvedFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_resolved_free(ptr, 0);
    }
    /**
     * End of the match, exclusive, in UTF-16 code units.
     * @returns {number}
     */
    get end() {
        const ret = wasm.resolved_end(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * True when no occurrence agreed with the recorded context and this is
     * merely the first one — very likely the wrong paragraph. The JS original
     * could not report this: it returned a bare index either way.
     * @returns {boolean}
     */
    get fallback() {
        const ret = wasm.resolved_fallback(this.__wbg_ptr);
        return ret !== 0;
    }
    /**
     * Start of the match, in UTF-16 code units — directly comparable to a JS
     * string index, which is what `highlightQuote` walks text nodes against.
     * @returns {number}
     */
    get start() {
        const ret = wasm.resolved_start(this.__wbg_ptr);
        return ret >>> 0;
    }
}
if (Symbol.dispose) Resolved.prototype[Symbol.dispose] = Resolved.prototype.free;

/**
 * Width of the context window, in UTF-16 code units.
 *
 * Exported so `captureSelector` records exactly as much context as resolution
 * compares. Duplicating the constant in JS is how the two silently disagree
 * about long prefixes.
 * @returns {number}
 */
export function contextUnits() {
    const ret = wasm.contextUnits();
    return ret >>> 0;
}

/**
 * Resolve a quote against `haystack`, the flattened text of the document.
 *
 * `prefix`/`suffix` arrive as `Uint16Array` rather than strings on purpose: the
 * context window is a fixed number of UTF-16 code units and can bisect a
 * surrogate pair, and a lone surrogate has no UTF-8 encoding to cross a `&str`
 * boundary with — wasm-bindgen would substitute `U+FFFD` and the prefix would
 * quietly stop matching. See the crate docs.
 *
 * `haystack` and `exact` stay strings because both come from well-formed
 * sources: parsed document text, and `Selection::toString()`.
 *
 * Pass `undefined` (or an empty array) for context that was not recorded; that
 * disables the corresponding half of disambiguation, matching how the daemon
 * omits absent fields.
 * @param {string} haystack
 * @param {string} exact
 * @param {Uint16Array | null} [prefix]
 * @param {Uint16Array | null} [suffix]
 * @returns {Resolved | undefined}
 */
export function resolveQuote(haystack, exact, prefix, suffix) {
    const ptr0 = passStringToWasm0(haystack, wasm.__wbindgen_export, wasm.__wbindgen_export2);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(exact, wasm.__wbindgen_export, wasm.__wbindgen_export2);
    const len1 = WASM_VECTOR_LEN;
    var ptr2 = isLikeNone(prefix) ? 0 : passArray16ToWasm0(prefix, wasm.__wbindgen_export);
    var len2 = WASM_VECTOR_LEN;
    var ptr3 = isLikeNone(suffix) ? 0 : passArray16ToWasm0(suffix, wasm.__wbindgen_export);
    var len3 = WASM_VECTOR_LEN;
    const ret = wasm.resolveQuote(ptr0, len0, ptr1, len1, ptr2, len2, ptr3, len3);
    return ret === 0 ? undefined : Resolved.__wrap(ret);
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg___wbindgen_throw_9c31b086c2b26051: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
    };
    return {
        __proto__: null,
        "./anchor_bg.js": import0,
    };
}

const ResolvedFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_resolved_free(ptr, 1));

function getStringFromWasm0(ptr, len) {
    return decodeText(ptr >>> 0, len);
}

let cachedUint16ArrayMemory0 = null;
function getUint16ArrayMemory0() {
    if (cachedUint16ArrayMemory0 === null || cachedUint16ArrayMemory0.byteLength === 0) {
        cachedUint16ArrayMemory0 = new Uint16Array(wasm.memory.buffer);
    }
    return cachedUint16ArrayMemory0;
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function isLikeNone(x) {
    return x === undefined || x === null;
}

function passArray16ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 2, 2) >>> 0;
    getUint16ArrayMemory0().set(arg, ptr / 2);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(ptr, len) {
    numBytesDecoded += len;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true, fatal: true });
        cachedTextDecoder.decode();
        numBytesDecoded = len;
    }
    return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedUint16ArrayMemory0 = null;
    cachedUint8ArrayMemory0 = null;
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = module.ok && expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;


    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('anchor_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
