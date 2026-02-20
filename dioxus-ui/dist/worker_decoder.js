let wasm_bindgen = (function(exports) {
    let script_src;
    if (typeof document !== 'undefined' && document.currentScript !== null) {
        script_src = new URL(document.currentScript.src, location.href).toString();
    }

    function main() {
        wasm.main();
    }
    exports.main = main;

    function __wbg_get_imports() {
        const import0 = {
            __proto__: null,
            __wbg_Error_8c4e43fe74559d73: function(arg0, arg1) {
                const ret = Error(getStringFromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_Number_04624de7d0e8332d: function(arg0) {
                const ret = Number(arg0);
                return ret;
            },
            __wbg___wbindgen_bigint_get_as_i64_8fcf4ce7f1ca72a2: function(arg0, arg1) {
                const v = arg1;
                const ret = typeof(v) === 'bigint' ? v : undefined;
                getDataViewMemory0().setBigInt64(arg0 + 8 * 1, isLikeNone(ret) ? BigInt(0) : ret, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
            },
            __wbg___wbindgen_boolean_get_bbbb1c18aa2f5e25: function(arg0) {
                const v = arg0;
                const ret = typeof(v) === 'boolean' ? v : undefined;
                return isLikeNone(ret) ? 0xFFFFFF : ret ? 1 : 0;
            },
            __wbg___wbindgen_debug_string_0bc8482c6e3508ae: function(arg0, arg1) {
                const ret = debugString(arg1);
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg___wbindgen_in_47fa6863be6f2f25: function(arg0, arg1) {
                const ret = arg0 in arg1;
                return ret;
            },
            __wbg___wbindgen_is_bigint_31b12575b56f32fc: function(arg0) {
                const ret = typeof(arg0) === 'bigint';
                return ret;
            },
            __wbg___wbindgen_is_function_0095a73b8b156f76: function(arg0) {
                const ret = typeof(arg0) === 'function';
                return ret;
            },
            __wbg___wbindgen_is_object_5ae8e5880f2c1fbd: function(arg0) {
                const val = arg0;
                const ret = typeof(val) === 'object' && val !== null;
                return ret;
            },
            __wbg___wbindgen_is_string_cd444516edc5b180: function(arg0) {
                const ret = typeof(arg0) === 'string';
                return ret;
            },
            __wbg___wbindgen_is_undefined_9e4d92534c42d778: function(arg0) {
                const ret = arg0 === undefined;
                return ret;
            },
            __wbg___wbindgen_jsval_eq_11888390b0186270: function(arg0, arg1) {
                const ret = arg0 === arg1;
                return ret;
            },
            __wbg___wbindgen_jsval_loose_eq_9dd77d8cd6671811: function(arg0, arg1) {
                const ret = arg0 == arg1;
                return ret;
            },
            __wbg___wbindgen_number_get_8ff4255516ccad3e: function(arg0, arg1) {
                const obj = arg1;
                const ret = typeof(obj) === 'number' ? obj : undefined;
                getDataViewMemory0().setFloat64(arg0 + 8 * 1, isLikeNone(ret) ? 0 : ret, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
            },
            __wbg___wbindgen_shr_ef8e07cce709eb54: function(arg0, arg1) {
                const ret = arg0 >> arg1;
                return ret;
            },
            __wbg___wbindgen_string_get_72fb696202c56729: function(arg0, arg1) {
                const obj = arg1;
                const ret = typeof(obj) === 'string' ? obj : undefined;
                var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                var len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg___wbindgen_throw_be289d5034ed271b: function(arg0, arg1) {
                throw new Error(getStringFromWasm0(arg0, arg1));
            },
            __wbg__wbg_cb_unref_d9b87ff7982e3b21: function(arg0) {
                arg0._wbg_cb_unref();
            },
            __wbg_call_389efe28435a9388: function() { return handleError(function (arg0, arg1) {
                const ret = arg0.call(arg1);
                return ret;
            }, arguments); },
            __wbg_close_46d119ceda5adf60: function(arg0) {
                arg0.close();
            },
            __wbg_close_edb842cf47c84609: function() { return handleError(function (arg0) {
                arg0.close();
            }, arguments); },
            __wbg_configure_eebd1783ab7547fc: function() { return handleError(function (arg0, arg1) {
                arg0.configure(arg1);
            }, arguments); },
            __wbg_createTask_deeb88a68fc97c9d: function() { return handleError(function (arg0, arg1) {
                const ret = console.createTask(getStringFromWasm0(arg0, arg1));
                return ret;
            }, arguments); },
            __wbg_data_5330da50312d0bc1: function(arg0) {
                const ret = arg0.data;
                return ret;
            },
            __wbg_decode_2790e16d13fc7dfa: function() { return handleError(function (arg0, arg1) {
                arg0.decode(arg1);
            }, arguments); },
            __wbg_done_57b39ecd9addfe81: function(arg0) {
                const ret = arg0.done;
                return ret;
            },
            __wbg_entries_58c7934c745daac7: function(arg0) {
                const ret = Object.entries(arg0);
                return ret;
            },
            __wbg_error_7534b8e9a36f1ab4: function(arg0, arg1) {
                let deferred0_0;
                let deferred0_1;
                try {
                    deferred0_0 = arg0;
                    deferred0_1 = arg1;
                    console.error(getStringFromWasm0(arg0, arg1));
                } finally {
                    wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
                }
            },
            __wbg_error_9a7fe3f932034cde: function(arg0) {
                console.error(arg0);
            },
            __wbg_get_9b94d73e6221f75c: function(arg0, arg1) {
                const ret = arg0[arg1 >>> 0];
                return ret;
            },
            __wbg_get_b3ed3ad4be2bc8ac: function() { return handleError(function (arg0, arg1) {
                const ret = Reflect.get(arg0, arg1);
                return ret;
            }, arguments); },
            __wbg_get_with_ref_key_1dc361bd10053bfe: function(arg0, arg1) {
                const ret = arg0[arg1];
                return ret;
            },
            __wbg_instanceof_ArrayBuffer_c367199e2fa2aa04: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof ArrayBuffer;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_instanceof_DedicatedWorkerGlobalScope_1506b3d017799d48: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof DedicatedWorkerGlobalScope;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_instanceof_Uint8Array_9b9075935c74707c: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof Uint8Array;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_instanceof_VideoFrame_479b1a8fd2597d2a: function(arg0) {
                let result;
                try {
                    result = arg0 instanceof VideoFrame;
                } catch (_) {
                    result = false;
                }
                const ret = result;
                return ret;
            },
            __wbg_isArray_d314bb98fcf08331: function(arg0) {
                const ret = Array.isArray(arg0);
                return ret;
            },
            __wbg_isSafeInteger_bfbc7332a9768d2a: function(arg0) {
                const ret = Number.isSafeInteger(arg0);
                return ret;
            },
            __wbg_iterator_6ff6560ca1568e55: function() {
                const ret = Symbol.iterator;
                return ret;
            },
            __wbg_length_32ed9a279acd054c: function(arg0) {
                const ret = arg0.length;
                return ret;
            },
            __wbg_length_35a7bace40f36eac: function(arg0) {
                const ret = arg0.length;
                return ret;
            },
            __wbg_log_6b5ca2e6124b2808: function(arg0) {
                console.log(arg0);
            },
            __wbg_new_361308b2356cecd0: function() {
                const ret = new Object();
                return ret;
            },
            __wbg_new_8a6f238a6ece86ea: function() {
                const ret = new Error();
                return ret;
            },
            __wbg_new_dd2b680c8bf6ae29: function(arg0) {
                const ret = new Uint8Array(arg0);
                return ret;
            },
            __wbg_new_df2fec5bace9c28f: function() { return handleError(function (arg0) {
                const ret = new EncodedVideoChunk(arg0);
                return ret;
            }, arguments); },
            __wbg_new_f23d87099c646aa7: function() { return handleError(function (arg0) {
                const ret = new VideoDecoder(arg0);
                return ret;
            }, arguments); },
            __wbg_new_from_slice_a3d2629dc1826784: function(arg0, arg1) {
                const ret = new Uint8Array(getArrayU8FromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_new_no_args_1c7c842f08d00ebb: function(arg0, arg1) {
                const ret = new Function(getStringFromWasm0(arg0, arg1));
                return ret;
            },
            __wbg_next_3482f54c49e8af19: function() { return handleError(function (arg0) {
                const ret = arg0.next();
                return ret;
            }, arguments); },
            __wbg_next_418f80d8f5303233: function(arg0) {
                const ret = arg0.next;
                return ret;
            },
            __wbg_now_a3af9a2f4bbaa4d1: function() {
                const ret = Date.now();
                return ret;
            },
            __wbg_postMessage_2041f4e90af61318: function() { return handleError(function (arg0, arg1) {
                arg0.postMessage(arg1);
            }, arguments); },
            __wbg_prototypesetcall_bdcdcc5842e4d77d: function(arg0, arg1, arg2) {
                Uint8Array.prototype.set.call(getArrayU8FromWasm0(arg0, arg1), arg2);
            },
            __wbg_queueMicrotask_0aa0a927f78f5d98: function(arg0) {
                const ret = arg0.queueMicrotask;
                return ret;
            },
            __wbg_queueMicrotask_5bb536982f78a56f: function(arg0) {
                queueMicrotask(arg0);
            },
            __wbg_resolve_002c4b7d9d8f6b64: function(arg0) {
                const ret = Promise.resolve(arg0);
                return ret;
            },
            __wbg_run_bcde7ea43ea6ed7c: function(arg0, arg1, arg2) {
                try {
                    var state0 = {a: arg1, b: arg2};
                    var cb0 = () => {
                        const a = state0.a;
                        state0.a = 0;
                        try {
                            return wasm_bindgen__convert__closures_____invoke__ha038f5592f0ab128(a, state0.b, );
                        } finally {
                            state0.a = a;
                        }
                    };
                    const ret = arg0.run(cb0);
                    return ret;
                } finally {
                    state0.a = state0.b = 0;
                }
            },
            __wbg_setInterval_1c7830d9e05ef97e: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = arg0.setInterval(arg1, arg2);
                return ret;
            }, arguments); },
            __wbg_setTimeout_e0aacd5a637418a6: function() { return handleError(function (arg0, arg1, arg2) {
                const ret = arg0.setTimeout(arg1, arg2);
                return ret;
            }, arguments); },
            __wbg_set_3f1d0b984ed272ed: function(arg0, arg1, arg2) {
                arg0[arg1] = arg2;
            },
            __wbg_set_codec_4a07f13d246fd92c: function(arg0, arg1, arg2) {
                arg0.codec = getStringFromWasm0(arg1, arg2);
            },
            __wbg_set_data_79bb5413d99467d2: function(arg0, arg1) {
                arg0.data = arg1;
            },
            __wbg_set_error_aac48fbbe176030a: function(arg0, arg1) {
                arg0.error = arg1;
            },
            __wbg_set_onmessage_ea7cc2ba099021cd: function(arg0, arg1) {
                arg0.onmessage = arg1;
            },
            __wbg_set_output_fa910612320d933c: function(arg0, arg1) {
                arg0.output = arg1;
            },
            __wbg_set_timestamp_f1dabd36612035b2: function(arg0, arg1) {
                arg0.timestamp = arg1;
            },
            __wbg_set_type_580cc65da7b24d1a: function(arg0, arg1) {
                arg0.type = __wbindgen_enum_EncodedVideoChunkType[arg1];
            },
            __wbg_stack_0ed75d68575b0f3c: function(arg0, arg1) {
                const ret = arg1.stack;
                const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
                const len1 = WASM_VECTOR_LEN;
                getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
                getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
            },
            __wbg_static_accessor_GLOBAL_12837167ad935116: function() {
                const ret = typeof global === 'undefined' ? null : global;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_GLOBAL_THIS_e628e89ab3b1c95f: function() {
                const ret = typeof globalThis === 'undefined' ? null : globalThis;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_SELF_a621d3dfbb60d0ce: function() {
                const ret = typeof self === 'undefined' ? null : self;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_static_accessor_WINDOW_f8727f0cf888e0bd: function() {
                const ret = typeof window === 'undefined' ? null : window;
                return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
            },
            __wbg_then_b9e7b3b5f1a9e1b5: function(arg0, arg1) {
                const ret = arg0.then(arg1);
                return ret;
            },
            __wbg_value_0546255b415e96c1: function(arg0) {
                const ret = arg0.value;
                return ret;
            },
            __wbindgen_cast_0000000000000001: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { dtor_idx: 1, function: Function { arguments: [], shim_idx: 43, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h55796e8105d5026a, wasm_bindgen__convert__closures_____invoke__h075de6cd4b0918de);
                return ret;
            },
            __wbindgen_cast_0000000000000002: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { dtor_idx: 61, function: Function { arguments: [NamedExternref("MessageEvent")], shim_idx: 65, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h34bd1d09bbaf7093, wasm_bindgen__convert__closures_____invoke__h14711d142b785476);
                return ret;
            },
            __wbindgen_cast_0000000000000003: function(arg0, arg1) {
                // Cast intrinsic for `Closure(Closure { dtor_idx: 85, function: Function { arguments: [Externref], shim_idx: 86, ret: Unit, inner_ret: Some(Unit) }, mutable: true }) -> Externref`.
                const ret = makeMutClosure(arg0, arg1, wasm.wasm_bindgen__closure__destroy__h242651b532b6b928, wasm_bindgen__convert__closures_____invoke__h05a1946733004b2d);
                return ret;
            },
            __wbindgen_cast_0000000000000004: function(arg0) {
                // Cast intrinsic for `F64 -> Externref`.
                const ret = arg0;
                return ret;
            },
            __wbindgen_cast_0000000000000005: function(arg0, arg1) {
                // Cast intrinsic for `Ref(String) -> Externref`.
                const ret = getStringFromWasm0(arg0, arg1);
                return ret;
            },
            __wbindgen_cast_0000000000000006: function(arg0) {
                // Cast intrinsic for `U64 -> Externref`.
                const ret = BigInt.asUintN(64, arg0);
                return ret;
            },
            __wbindgen_init_externref_table: function() {
                const table = wasm.__wbindgen_externrefs;
                const offset = table.grow(4);
                table.set(0, undefined);
                table.set(offset + 0, undefined);
                table.set(offset + 1, null);
                table.set(offset + 2, true);
                table.set(offset + 3, false);
            },
        };
        return {
            __proto__: null,
            "./worker_decoder_bg.js": import0,
        };
    }

    function wasm_bindgen__convert__closures_____invoke__h075de6cd4b0918de(arg0, arg1) {
        wasm.wasm_bindgen__convert__closures_____invoke__h075de6cd4b0918de(arg0, arg1);
    }

    function wasm_bindgen__convert__closures_____invoke__ha038f5592f0ab128(arg0, arg1) {
        const ret = wasm.wasm_bindgen__convert__closures_____invoke__ha038f5592f0ab128(arg0, arg1);
        return ret !== 0;
    }

    function wasm_bindgen__convert__closures_____invoke__h14711d142b785476(arg0, arg1, arg2) {
        wasm.wasm_bindgen__convert__closures_____invoke__h14711d142b785476(arg0, arg1, arg2);
    }

    function wasm_bindgen__convert__closures_____invoke__h05a1946733004b2d(arg0, arg1, arg2) {
        wasm.wasm_bindgen__convert__closures_____invoke__h05a1946733004b2d(arg0, arg1, arg2);
    }


    const __wbindgen_enum_EncodedVideoChunkType = ["key", "delta"];

    function addToExternrefTable0(obj) {
        const idx = wasm.__externref_table_alloc();
        wasm.__wbindgen_externrefs.set(idx, obj);
        return idx;
    }

    const CLOSURE_DTORS = (typeof FinalizationRegistry === 'undefined')
        ? { register: () => {}, unregister: () => {} }
        : new FinalizationRegistry(state => state.dtor(state.a, state.b));

    function debugString(val) {
        // primitive types
        const type = typeof val;
        if (type == 'number' || type == 'boolean' || val == null) {
            return  `${val}`;
        }
        if (type == 'string') {
            return `"${val}"`;
        }
        if (type == 'symbol') {
            const description = val.description;
            if (description == null) {
                return 'Symbol';
            } else {
                return `Symbol(${description})`;
            }
        }
        if (type == 'function') {
            const name = val.name;
            if (typeof name == 'string' && name.length > 0) {
                return `Function(${name})`;
            } else {
                return 'Function';
            }
        }
        // objects
        if (Array.isArray(val)) {
            const length = val.length;
            let debug = '[';
            if (length > 0) {
                debug += debugString(val[0]);
            }
            for(let i = 1; i < length; i++) {
                debug += ', ' + debugString(val[i]);
            }
            debug += ']';
            return debug;
        }
        // Test for built-in
        const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
        let className;
        if (builtInMatches && builtInMatches.length > 1) {
            className = builtInMatches[1];
        } else {
            // Failed to match the standard '[object ClassName]'
            return toString.call(val);
        }
        if (className == 'Object') {
            // we're a user defined class or Object
            // JSON.stringify avoids problems with cycles, and is generally much
            // easier than looping through ownProperties of `val`.
            try {
                return 'Object(' + JSON.stringify(val) + ')';
            } catch (_) {
                return 'Object';
            }
        }
        // errors
        if (val instanceof Error) {
            return `${val.name}: ${val.message}\n${val.stack}`;
        }
        // TODO we could test for more things here, like `Set`s and `Map`s.
        return className;
    }

    function getArrayU8FromWasm0(ptr, len) {
        ptr = ptr >>> 0;
        return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
    }

    let cachedDataViewMemory0 = null;
    function getDataViewMemory0() {
        if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || (cachedDataViewMemory0.buffer.detached === undefined && cachedDataViewMemory0.buffer !== wasm.memory.buffer)) {
            cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
        }
        return cachedDataViewMemory0;
    }

    function getStringFromWasm0(ptr, len) {
        ptr = ptr >>> 0;
        return decodeText(ptr, len);
    }

    let cachedUint8ArrayMemory0 = null;
    function getUint8ArrayMemory0() {
        if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
            cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
        }
        return cachedUint8ArrayMemory0;
    }

    function handleError(f, args) {
        try {
            return f.apply(this, args);
        } catch (e) {
            const idx = addToExternrefTable0(e);
            wasm.__wbindgen_exn_store(idx);
        }
    }

    function isLikeNone(x) {
        return x === undefined || x === null;
    }

    function makeMutClosure(arg0, arg1, dtor, f) {
        const state = { a: arg0, b: arg1, cnt: 1, dtor };
        const real = (...args) => {

            // First up with a closure we increment the internal reference
            // count. This ensures that the Rust closure environment won't
            // be deallocated while we're invoking it.
            state.cnt++;
            const a = state.a;
            state.a = 0;
            try {
                return f(a, state.b, ...args);
            } finally {
                state.a = a;
                real._wbg_cb_unref();
            }
        };
        real._wbg_cb_unref = () => {
            if (--state.cnt === 0) {
                state.dtor(state.a, state.b);
                state.a = 0;
                CLOSURE_DTORS.unregister(state);
            }
        };
        CLOSURE_DTORS.register(real, state, state);
        return real;
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
    function decodeText(ptr, len) {
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

    let wasmModule, wasm;
    function __wbg_finalize_init(instance, module) {
        wasm = instance.exports;
        wasmModule = module;
        cachedDataViewMemory0 = null;
        cachedUint8ArrayMemory0 = null;
        wasm.__wbindgen_start();
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

        if (module_or_path === undefined && script_src !== undefined) {
            module_or_path = script_src.replace(/\.js$/, "_bg.wasm");
        }
        const imports = __wbg_get_imports();

        if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
            module_or_path = fetch(module_or_path);
        }

        const { instance, module } = await __wbg_load(await module_or_path, imports);

        return __wbg_finalize_init(instance, module);
    }

    return Object.assign(__wbg_init, { initSync }, exports);
})({ __proto__: null });
