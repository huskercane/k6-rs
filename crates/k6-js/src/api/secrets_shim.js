globalThis.SecretVault = function(config) {
    this._env = config.env || {};
    this._file = config.file || {};
    this._cache = {};
};

SecretVault.prototype.get = function(name) {
    if (this._cache[name] !== undefined) {
        return this._cache[name];
    }

    // Check env-based secrets first
    if (this._env[name] !== undefined) {
        var envVar = this._env[name];
        var val = __ENV[envVar];
        if (val === undefined) {
            throw new Error('secret "' + name + '": env var "' + envVar + '" not found');
        }
        this._cache[name] = val;
        return val;
    }

    // Check file-based secrets
    if (this._file[name] !== undefined) {
        var path = this._file[name];
        var content = __secrets_read_file(path);
        this._cache[name] = content;
        return content;
    }

    throw new Error('unknown secret "' + name + '"');
};
