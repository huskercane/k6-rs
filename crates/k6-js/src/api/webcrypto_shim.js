(function() {
    // Helper: string to hex
    function strToHex(str) {
        var hex = '';
        for (var i = 0; i < str.length; i++) {
            var code = str.charCodeAt(i);
            hex += ('0' + code.toString(16)).slice(-2);
        }
        return hex;
    }

    // Helper: hex to string
    function hexToStr(hex) {
        var str = '';
        for (var i = 0; i < hex.length; i += 2) {
            str += String.fromCharCode(parseInt(hex.substr(i, 2), 16));
        }
        return str;
    }

    // Helper: Uint8Array to hex
    function u8ToHex(arr) {
        var hex = '';
        for (var i = 0; i < arr.length; i++) {
            hex += ('0' + arr[i].toString(16)).slice(-2);
        }
        return hex;
    }

    // Helper: hex to Uint8Array
    function hexToU8(hex) {
        var arr = new Uint8Array(hex.length / 2);
        for (var i = 0; i < hex.length; i += 2) {
            arr[i / 2] = parseInt(hex.substr(i, 2), 16);
        }
        return arr;
    }

    // Helper: convert data argument to hex (accepts string, Uint8Array, or hex)
    function toHex(data) {
        if (typeof data === 'string') {
            return strToHex(data);
        }
        if (data instanceof Uint8Array || (data && typeof data.length === 'number' && typeof data[0] === 'number')) {
            return u8ToHex(data);
        }
        return strToHex(String(data));
    }

    // CryptoKey object
    function CryptoKey(type, algorithm, extractable, usages, _rawHex) {
        this.type = type;
        this.algorithm = algorithm;
        this.extractable = extractable;
        this.usages = usages;
        this._rawHex = _rawHex;
    }

    var subtle = {
        digest: function(algorithm, data) {
            var algo = typeof algorithm === 'string' ? algorithm : algorithm.name;
            var dataHex = toHex(data);
            return __wc_digest(algo, dataHex);
        },

        generateKey: function(algorithm, extractable, keyUsages) {
            var name = algorithm.name;
            var length = algorithm.length || 256;

            if (name === 'HMAC') {
                var hashName = typeof algorithm.hash === 'string' ? algorithm.hash : algorithm.hash.name;
                var keyLen = algorithm.length || (hashName === 'SHA-512' ? 64 : hashName === 'SHA-384' ? 48 : 32);
                var rawHex = __wc_random_bytes(keyLen);
                return new CryptoKey('secret', { name: 'HMAC', hash: { name: hashName }, length: keyLen * 8 }, extractable, keyUsages, rawHex);
            }

            if (name === 'AES-GCM' || name === 'AES-CBC' || name === 'AES-CTR') {
                var keyLen = length / 8;
                var rawHex = __wc_random_bytes(keyLen);
                return new CryptoKey('secret', { name: name, length: length }, extractable, keyUsages, rawHex);
            }

            throw new Error('generateKey: unsupported algorithm: ' + name);
        },

        importKey: function(format, keyData, algorithm, extractable, keyUsages) {
            if (format !== 'raw') throw new Error('importKey: only "raw" format supported');
            var name = typeof algorithm === 'string' ? algorithm : algorithm.name;
            var rawHex = toHex(keyData);
            var algoObj;

            if (name === 'HMAC') {
                var hashName = algorithm.hash ? (typeof algorithm.hash === 'string' ? algorithm.hash : algorithm.hash.name) : 'SHA-256';
                algoObj = { name: 'HMAC', hash: { name: hashName } };
            } else if (name === 'PBKDF2') {
                algoObj = { name: 'PBKDF2' };
            } else if (name === 'AES-GCM' || name === 'AES-CBC' || name === 'AES-CTR') {
                algoObj = { name: name, length: rawHex.length * 4 };
            } else {
                algoObj = { name: name };
            }

            return new CryptoKey('secret', algoObj, extractable, keyUsages, rawHex);
        },

        exportKey: function(format, key) {
            if (format !== 'raw') throw new Error('exportKey: only "raw" format supported');
            if (!key.extractable) throw new Error('exportKey: key is not extractable');
            return hexToU8(key._rawHex);
        },

        sign: function(algorithm, key, data) {
            var name = typeof algorithm === 'string' ? algorithm : algorithm.name;
            var dataHex = toHex(data);

            if (name === 'HMAC') {
                var hashName = key.algorithm.hash ? key.algorithm.hash.name : 'SHA-256';
                return __wc_hmac_sign(hashName, key._rawHex, dataHex);
            }

            throw new Error('sign: unsupported algorithm: ' + name);
        },

        verify: function(algorithm, key, signature, data) {
            var name = typeof algorithm === 'string' ? algorithm : algorithm.name;
            var sigHex = typeof signature === 'string' ? signature : toHex(signature);
            var dataHex = toHex(data);

            if (name === 'HMAC') {
                var hashName = key.algorithm.hash ? key.algorithm.hash.name : 'SHA-256';
                return __wc_hmac_verify(hashName, key._rawHex, sigHex, dataHex);
            }

            throw new Error('verify: unsupported algorithm: ' + name);
        },

        encrypt: function(algorithm, key, data) {
            var name = algorithm.name;
            var dataHex = toHex(data);

            if (name === 'AES-GCM') {
                var ivHex = u8ToHex(algorithm.iv);
                return __wc_aes_gcm_encrypt(key._rawHex, ivHex, dataHex);
            }
            if (name === 'AES-CBC') {
                var ivHex = u8ToHex(algorithm.iv);
                return __wc_aes_cbc_encrypt(key._rawHex, ivHex, dataHex);
            }
            if (name === 'AES-CTR') {
                var counterHex = u8ToHex(algorithm.counter);
                return __wc_aes_ctr_encrypt(key._rawHex, counterHex, dataHex);
            }
            throw new Error('encrypt: unsupported algorithm: ' + name);
        },

        decrypt: function(algorithm, key, data) {
            var name = algorithm.name;
            var dataHex = typeof data === 'string' ? data : toHex(data);

            if (name === 'AES-GCM') {
                var ivHex = u8ToHex(algorithm.iv);
                var plainHex = __wc_aes_gcm_decrypt(key._rawHex, ivHex, dataHex);
                return hexToStr(plainHex);
            }
            if (name === 'AES-CBC') {
                var ivHex = u8ToHex(algorithm.iv);
                var plainHex = __wc_aes_cbc_decrypt(key._rawHex, ivHex, dataHex);
                return hexToStr(plainHex);
            }
            if (name === 'AES-CTR') {
                var counterHex = u8ToHex(algorithm.counter);
                var plainHex = __wc_aes_ctr_decrypt(key._rawHex, counterHex, dataHex);
                return hexToStr(plainHex);
            }
            throw new Error('decrypt: unsupported algorithm: ' + name);
        },

        deriveBits: function(algorithm, baseKey, length) {
            var name = algorithm.name;
            if (name === 'PBKDF2') {
                var saltHex = toHex(algorithm.salt);
                var hashName = typeof algorithm.hash === 'string' ? algorithm.hash : algorithm.hash.name;
                return __wc_pbkdf2(baseKey._rawHex, saltHex, algorithm.iterations, hashName, length);
            }
            throw new Error('deriveBits: unsupported algorithm: ' + name);
        },

        deriveKey: function(algorithm, baseKey, derivedKeyAlgorithm, extractable, keyUsages) {
            var bits = derivedKeyAlgorithm.length || 256;
            var rawHex = this.deriveBits(algorithm, baseKey, bits);
            return new CryptoKey('secret', derivedKeyAlgorithm, extractable, keyUsages, rawHex);
        }
    };

    // Merge with existing crypto global (k6/crypto has sha256, md5, etc.)
    var existing = globalThis.crypto || {};
    existing.subtle = subtle;
    existing.getRandomValues = function(array) {
        var hex = __wc_random_bytes(array.length);
        for (var i = 0; i < array.length; i++) {
            array[i] = parseInt(hex.substr(i * 2, 2), 16);
        }
        return array;
    };
    globalThis.crypto = existing;
})();
