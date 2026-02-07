//
// Copyright(C) 1993-1996 Id Software, Inc.
// Copyright(C) 2005-2014 Simon Howard
//
// This program is free software; you can redistribute it and/or
// modify it under the terms of the GNU General Public License
// as published by the Free Software Foundation; either version 2
// of the License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// DESCRIPTION:
//	WAD I/O functions.
//

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "m_misc.h"
#include "w_file.h"
#include "z_zone.h"

typedef struct
{
    wad_file_t wad;
    FILE *fstream;
} stdc_wad_file_t;

extern wad_file_class_t stdc_wad_file;

static wad_file_t *W_StdC_OpenFile(char *path)
{
    stdc_wad_file_t *result;
    FILE *fstream;

    fstream = fopen(path, "rb");

    if (fstream == NULL)
    {
        return NULL;
    }

    // Create a new stdc_wad_file_t to hold the file handle.

    result = Z_Malloc(sizeof(stdc_wad_file_t), PU_STATIC, 0);
    result->wad.file_class = &stdc_wad_file;
    result->wad.length = M_FileLength(fstream);
    result->fstream = fstream;

    // Read the entire WAD into memory to avoid re-reading the full file
    // on every lump access (Akuma's sys_read reads the whole file each time).
    printf("[WAD] Loading %u bytes of %s into memory...\n",
           result->wad.length, path);

    result->wad.mapped = malloc(result->wad.length);
    if (result->wad.mapped != NULL)
    {
        fseek(fstream, 0, SEEK_SET);
        size_t got = fread(result->wad.mapped, 1, result->wad.length, fstream);
        if (got < result->wad.length)
        {
            printf("[WAD] Warning: only read %u of %u bytes\n",
                   (unsigned)got, result->wad.length);
        }
        printf("[WAD] WAD loaded into memory at %p\n", result->wad.mapped);
    }
    else
    {
        // Fall back to file I/O if malloc fails
        result->wad.mapped = NULL;
        printf("[WAD] Could not allocate memory, using file I/O\n");
    }

    return &result->wad;
}

static void W_StdC_CloseFile(wad_file_t *wad)
{
    stdc_wad_file_t *stdc_wad;

    stdc_wad = (stdc_wad_file_t *) wad;

    if (stdc_wad->wad.mapped != NULL)
    {
        free(stdc_wad->wad.mapped);
        stdc_wad->wad.mapped = NULL;
    }

    fclose(stdc_wad->fstream);
    Z_Free(stdc_wad);
}

// Read data from the specified position in the file into the 
// provided buffer.  Returns the number of bytes read.

size_t W_StdC_Read(wad_file_t *wad, unsigned int offset,
                   void *buffer, size_t buffer_len)
{
    // If the WAD is memory-mapped, just memcpy from the mapped region
    if (wad->mapped != NULL)
    {
        if (offset >= wad->length)
            return 0;
        size_t available = wad->length - offset;
        size_t to_copy = buffer_len < available ? buffer_len : available;
        memcpy(buffer, (unsigned char *)wad->mapped + offset, to_copy);
        return to_copy;
    }

    // Fallback: read from file
    stdc_wad_file_t *stdc_wad;
    size_t result;

    stdc_wad = (stdc_wad_file_t *) wad;

    // Jump to the specified position in the file.

    fseek(stdc_wad->fstream, offset, SEEK_SET);

    // Read into the buffer.

    result = fread(buffer, 1, buffer_len, stdc_wad->fstream);

    return result;
}


wad_file_class_t stdc_wad_file = 
{
    W_StdC_OpenFile,
    W_StdC_CloseFile,
    W_StdC_Read,
};


