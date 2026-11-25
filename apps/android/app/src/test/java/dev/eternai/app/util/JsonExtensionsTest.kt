package dev.eternai.app.util

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class JsonExtensionsTest {

    @Test
    fun jsonNullIsNotConvertedToTheTextNull() {
        val obj = JSONObject().put("content", JSONObject.NULL)

        assertNull(obj.optStringOrNull("content"))
        assertNull(obj.optStringOrNull("missing"))
    }

    @Test
    fun stringValuesArePreserved() {
        val obj = JSONObject().put("content", "hello")

        assertEquals("hello", obj.optStringOrNull("content"))
    }
}
