globalThis.csv = {
    parse: function(csvString, options) {
        var delimiter = (options && options.delimiter) || ',';
        var lines = csvString.split('\n');
        var result = [];

        if (lines.length === 0) return result;

        // Parse header
        var headers = csv._parseLine(lines[0], delimiter);

        for (var i = 1; i < lines.length; i++) {
            var line = lines[i].trim();
            if (line === '') continue;
            var values = csv._parseLine(line, delimiter);
            var obj = {};
            for (var j = 0; j < headers.length; j++) {
                obj[headers[j]] = j < values.length ? values[j] : '';
            }
            result.push(obj);
        }

        return result;
    },

    _parseLine: function(line, delimiter) {
        var result = [];
        var current = '';
        var inQuotes = false;

        for (var i = 0; i < line.length; i++) {
            var ch = line[i];
            if (inQuotes) {
                if (ch === '"') {
                    if (i + 1 < line.length && line[i + 1] === '"') {
                        current += '"';
                        i++;
                    } else {
                        inQuotes = false;
                    }
                } else {
                    current += ch;
                }
            } else {
                if (ch === '"') {
                    inQuotes = true;
                } else if (ch === delimiter) {
                    result.push(current.trim());
                    current = '';
                } else {
                    current += ch;
                }
            }
        }
        result.push(current.trim());
        return result;
    }
};
