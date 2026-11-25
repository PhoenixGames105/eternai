package dev.eternai.app.util

import org.json.JSONObject

/** Returns a JSON string value, or null for missing, JSON-null, and non-string values. */
internal fun JSONObject.optStringOrNull(name: String): String? =
    opt(name) as? String
