globalThis.fs = {
    open: function(path) {
        // Eagerly read the file content (init context)
        var content = __fs_read(path);
        var statJson = __fs_stat(path);
        var statObj = JSON.parse(statJson);

        return {
            _path: path,
            _content: content,
            _stat: statObj,
            read: function() {
                return this._content;
            },
            stat: function() {
                return this._stat;
            }
        };
    },

    stat: function(path) {
        var statJson = __fs_stat(path);
        return JSON.parse(statJson);
    }
};
