package dev.eternai.app.sync

import dev.eternai.core.MessageDto
import dev.eternai.core.SyncTurnDto
import dev.eternai.core.TurnDto
import org.junit.Assert.assertEquals
import org.junit.Test

class LocalContextTest {

    @Test
    fun replacingEdgeWindowDoesNotRemoveVisibleTranscriptHistory() {
        val context = LocalContext({})
        context.setHistory(listOf(turn("old", "old transcript message")))

        context.setWindow(listOf(turn("recent", "recent context message")))
        context.append(
            SyncTurnDto(
                "pending",
                3,
                1,
                true,
                listOf(message("user", "unsynced phone message")),
            ),
        )

        assertEquals(listOf("old", "pending"), context.turns.value.map { it.turnId })
        assertEquals(
            listOf("recent context message", "unsynced phone message"),
            context.messages().map { it.content },
        )

        context.clearLocal()
        assertEquals(listOf("old"), context.turns.value.map { it.turnId })
    }

    @Test
    fun historyCanRebuildTheEdgeWindowFromDesktopMessages() {
        val context = LocalContext({})
        context.setHistory(
            listOf(
                turn("desktop-1", "remember 22"),
                turn("desktop-2", "I will remember"),
            ),
        )

        assertEquals(
            listOf("desktop-1", "desktop-2"),
            context.turnsForEdgeWindow().map { it.turnId },
        )
        assertEquals(
            listOf("remember 22", "I will remember"),
            context.turnsForEdgeWindow().flatMap { it.messages }.map { it.content },
        )
    }

    @Test
    fun startupRestoresPendingOutboxTurnsWithoutDuplicatingCachedHistory() {
        val context = LocalContext({})
        context.restoreHistory(listOf(turn("synced", "already on desktop")))
        context.restoreLocal(
            listOf(
                SyncTurnDto("synced", 1, 0, true, listOf(message("user", "already on desktop"))),
                SyncTurnDto("pending", 2, 1, true, listOf(message("user", "still waiting to sync"))),
            ),
        )

        assertEquals(listOf("synced", "pending"), context.turns.value.map { it.turnId })
        assertEquals(
            listOf("still waiting to sync"),
            context.messages().map { it.content },
        )
    }

    @Test
    fun edgeWindowSnapshotIncludesPendingTurnsAndAvoidsRepeatingSyncedOnes() {
        val context = LocalContext({})
        context.restoreHistory(listOf(turn("synced", "desktop remembered 22")))
        context.restoreLocal(
            listOf(
                SyncTurnDto("synced", 1, 0, true, listOf(message("user", "desktop remembered 22"))),
                SyncTurnDto(
                    "pending",
                    2,
                    1,
                    true,
                    listOf(message("user", "phone turn")),
                ),
            ),
        )

        assertEquals(
            listOf("synced", "pending"),
            context.turnsForEdgeWindow().map { it.turnId },
        )
        context.setWindow(context.turnsForEdgeWindow())
        assertEquals(
            listOf("desktop remembered 22", "phone turn"),
            context.messages().map { it.content },
        )
    }

    private fun turn(id: String, text: String) = TurnDto(
        id,
        1,
        "chat",
        "edge",
        "model",
        "phone",
        1,
        listOf(message("user", text)),
    )

    private fun message(role: String, text: String) = MessageDto(role, text, null, null, null)
}
