-- Load after the existing mouse move binding, using the same chord.
return function(chord, cli)
    local button = assert(chord:match("(mouse:%d+)%s*$"), "The detach chord must end with a mouse button")
    local id = os.time() * 1000000
    local drag
    local command = "'" .. cli:gsub("'", "'\\''") .. "' virtual-panel detach "

    hl.bind(chord, function()
        local window = hl.get_active_window()
        if not window or not window.address:match("^0x%x+$") then return end
        local cursor = hl.get_cursor_pos()
        id = id + 1
        drag = id
        hl.exec_cmd(command .. string.format("begin %d %s %d %d", drag, window.address,
            math.floor(cursor.x), math.floor(cursor.y)))
    end)

    hl.bind(button, function()
        if not drag then return end
        local cursor = hl.get_cursor_pos()
        hl.exec_cmd(command .. string.format("release %d %d %d", drag,
            math.floor(cursor.x), math.floor(cursor.y)))
        drag = nil
    -- The user may let go of the modifier before letting go of the mouse.
    end, { release = true, ignore_mods = true, non_consuming = true, transparent = true })
end
